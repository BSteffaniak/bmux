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

type ReplicationAdmission = Result<
    (String, NodeId, Option<LogIdentity>, Membership),
    bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError,
>;

/// Classified failures at the unpublished authenticated append boundary.
#[derive(Debug)]
pub enum AuthenticatedAppendError {
    Unauthorized,
    TermMismatch,
    MissingTransfer {
        reference: Box<bmux_cluster_plugin_api::cluster_consensus_rpc_types::SealedEntryReference>,
        term: Term,
    },
    Executor(VoteGateError),
    Storage(io::Error),
    Invalid(String),
}

impl From<String> for AuthenticatedAppendError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}

impl From<&str> for AuthenticatedAppendError {
    fn from(message: &str) -> Self {
        Self::Invalid(message.to_owned())
    }
}

/// Outcome of one bounded, caller-driven replication turn. Catch-up is log
/// replication only, not application completion or a linearizable read barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationTurn {
    CaughtUp,
    Yielded,
}

struct OutboundAppendBatch {
    v2: bool,
    endpoint: String,
    cluster: String,
    credential_serial: String,
    append: AppendRequest,
}

fn wire_log_identity(
    id: LogIdentity,
) -> bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity {
    bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity {
        term: id.term.0,
        index: id.index.0,
        leader: id.leader.as_bytes().to_vec(),
    }
}

fn verify_outbound_append(
    local: NodeId,
    request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
) -> Result<(), bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
    use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
    let signed = crate::consensus_network::owned_append_signing_payload(request)
        .map_err(|_| RpcError::InvalidRequest)?;
    if request.sender.as_slice() != local.as_bytes() || request.proof.signature.len() != 64 {
        return Err(RpcError::Unauthorized);
    }
    crate::membership::verify_node_signature(&local.to_string(), &signed, &request.proof.signature)
        .map_err(|_| RpcError::Unauthorized)
}

fn decode_replication_identity(
    id: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity,
) -> Result<LogIdentity, std::array::TryFromSliceError> {
    Ok(LogIdentity {
        term: Term(id.term),
        index: crate::consensus_model::LogIndex(id.index),
        leader: NodeId::from_bytes(id.leader.as_slice().try_into()?),
    })
}

fn owned_vote_endpoint(
    state: &crate::control_state::ControlState,
    cluster: &str,
    recipient: NodeId,
) -> Result<String, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
    use bmux_cluster_plugin_api::{
        cluster_consensus_rpc_types::RpcError, cluster_types::ClusterMemberState,
    };
    let member = state
        .members
        .get(&recipient.to_string())
        .ok_or(RpcError::Unauthorized)?;
    if member.cluster_id != cluster || member.state != ClusterMemberState::Active {
        return Err(RpcError::Unauthorized);
    }
    if !crate::consensus_network::supports_owned_consensus(member) {
        return Err(RpcError::Incompatible);
    }
    member
        .endpoint
        .as_ref()
        .filter(|endpoint| !endpoint.is_empty())
        .cloned()
        .ok_or(RpcError::Unauthorized)
}

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("owned_authority");
const EMPTY_POSITIONS: [&str; 5] = [
    "commit",
    "applied",
    "snapshot",
    "membership",
    "applied_membership",
];
const FORMAT: &[u8] = b"BMOWNV06";
// Install receipts are node-local protocol recovery authority, not application
// state copied from a remote snapshot. Retain the last 64 successful installs.
const INSTALL_RECEIPT_BYTES: usize = 160;
const MAX_INSTALL_RECEIPTS: usize = 64;
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
// Shared by local proposal admission and each bounded application turn. The
// budget counts the complete canonical log entry, not just command payload.
const APPLICATION_ENTRY_BUDGET: usize = 1024 * 1024;

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

// Installation duplicates the snapshot twice plus its canonical control and
// membership state, and may rebuild both suffix tables in a copy-on-write tx.
// Reserve 8 MiB for metadata/transaction bookkeeping, charge twice every new
// byte for page slack/COW, and admit only within half the backend ceiling to
// leave room for redb geometric file growth. This intentionally conservative
// prototype limit is NOT a negotiated snapshot-format/workspace-size limit.
fn snapshot_install_fits(existing: u64, snapshot: u64, membership: u64, suffix: u64) -> bool {
    snapshot
        .checked_mul(3)
        .and_then(|bytes| membership.checked_mul(3).and_then(|m| bytes.checked_add(m)))
        .and_then(|bytes| suffix.checked_mul(2).and_then(|s| bytes.checked_add(s)))
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(existing))
        .and_then(|bytes| bytes.checked_add(8 * 1024 * 1024))
        .is_some_and(|bytes| bytes <= MAX_VOTE_PAYLOAD / 2)
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

// Canonical control state is validated independently of retained log framing.
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
    let (descriptor, control) =
        crate::consensus_storage::decode_owned_snapshot(bytes.value()).map_err(io::Error::other)?;
    if snapshot == applied {
        let current = meta
            .get("control")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing installed control state"))?;
        if current.value() != control {
            return Err(invalid("installed snapshot/control mismatch"));
        }
    }
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

fn validate_install_receipts(bytes: &[u8]) -> io::Result<()> {
    if !bytes.len().is_multiple_of(INSTALL_RECEIPT_BYTES)
        || bytes.len() > MAX_INSTALL_RECEIPTS * INSTALL_RECEIPT_BYTES
    {
        return Err(invalid("invalid install receipt framing"));
    }
    for (index, receipt) in bytes
        .as_chunks::<INSTALL_RECEIPT_BYTES>()
        .0
        .iter()
        .enumerate()
    {
        let boundary = decode_position(&receipt[104..152])?
            .ok_or_else(|| invalid("missing receipt boundary"))?;
        let term = u64::from_le_bytes(receipt[152..160].try_into().map_err(io::Error::other)?);
        if boundary.term.0 > term
            || bytes[..index * INSTALL_RECEIPT_BYTES]
                .as_chunks::<INSTALL_RECEIPT_BYTES>()
                .0
                .iter()
                .any(|old| old[..48] == receipt[..48] || (old[16..72] == receipt[16..72]))
        {
            return Err(invalid("conflicting install receipt authority"));
        }
    }
    Ok(())
}

fn validate_install_receipt_coverage(
    bytes: &[u8],
    applied: Option<LogIdentity>,
    hard_state: HardState,
) -> io::Result<()> {
    validate_install_receipts(bytes)?;
    for receipt in bytes.as_chunks::<INSTALL_RECEIPT_BYTES>().0 {
        let boundary = decode_position(&receipt[104..152])?
            .ok_or_else(|| invalid("missing receipt boundary"))?;
        let term = Term(u64::from_le_bytes(
            receipt[152..160].try_into().map_err(io::Error::other)?,
        ));
        if term > hard_state.term
            || crate::consensus_model::validate_position_prefix(Some(boundary), applied).is_err()
            || applied.is_some_and(|applied| {
                applied.term == boundary.term && applied.leader != boundary.leader
            })
        {
            return Err(invalid(
                "install receipt exceeds durable applied or term authority",
            ));
        }
    }
    Ok(())
}

fn validate_read_install_receipts(meta: &redb::ReadOnlyTable<&str, &[u8]>) -> io::Result<()> {
    let get = |key| -> io::Result<Vec<u8>> {
        Ok(meta
            .get(key)
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing receipt authority"))?
            .value()
            .to_vec())
    };
    validate_install_receipt_coverage(
        &get("install_receipts")?,
        decode_position(&get("applied")?)?,
        decode(&get("vote")?)?,
    )
}

fn stored_membership(bytes: &[u8]) -> io::Result<crate::consensus_model::StoredMembership> {
    let entry = crate::consensus_storage::decode_owned_entry(bytes).map_err(io::Error::other)?;
    let EntryPayload::Membership(membership) = entry.payload else {
        return Err(invalid("expected membership entry"));
    };
    Ok(crate::consensus_model::StoredMembership {
        introduced_at: Some(entry.identity),
        membership,
    })
}

fn validate_owned_genesis(entry: &Entry) -> io::Result<()> {
    let EntryPayload::Membership(membership) = &entry.payload else {
        return Err(invalid("invalid genesis payload"));
    };
    crate::consensus_model::validate_membership_genesis(&crate::consensus_model::StoredMembership {
        introduced_at: Some(entry.identity),
        membership: membership.clone(),
    })
    .map_err(|_| invalid("invalid owned genesis membership"))
}

fn encode_owned_genesis(genesis: Option<Entry>) -> io::Result<Option<(LogIdentity, Vec<u8>)>> {
    let encoded = genesis
        .map(|entry| {
            validate_owned_genesis(&entry)?;
            crate::consensus_storage::encode_owned_entry(&entry)
                .map(|bytes| (entry.identity, bytes))
                .map_err(io::Error::other)
        })
        .transpose()?;
    if encoded
        .as_ref()
        .is_some_and(|(_, bytes)| bytes.len() > APPLICATION_ENTRY_BUDGET)
    {
        return Err(invalid("bootstrap membership exceeds size limit"));
    }
    Ok(encoded)
}

fn validate_owned_membership_chain(
    seed: Option<&crate::consensus_model::StoredMembership>,
    entries: &[crate::consensus_model::StoredMembership],
    confirmed: Option<LogIdentity>,
) -> io::Result<()> {
    let (previous, tail) = if let Some(seed) = seed {
        (seed, entries)
    } else if let Some((genesis, tail)) = entries.split_first() {
        crate::consensus_model::validate_membership_genesis(genesis)
            .map_err(|_| invalid("invalid owned membership genesis"))?;
        (genesis, tail)
    } else {
        return Ok(());
    };
    crate::consensus_model::validate_membership_sequence(previous, tail.iter(), confirmed)
        .map_err(|_| invalid("invalid owned membership transition chain"))
}

fn append_lookup_entries(
    log: &redb::Table<u64, &[u8]>,
    request: &AppendRequest,
    committed: Option<LogIdentity>,
) -> io::Result<std::collections::BTreeMap<u64, Entry>> {
    let mut entries = std::collections::BTreeMap::new();
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
    Ok(entries)
}

fn validate_candidate_memberships(
    meta: &redb::Table<&str, &[u8]>,
    memberships: &redb::Table<u64, &[u8]>,
    request: &AppendRequest,
    replace_from: Option<usize>,
    confirmed: Option<LogIdentity>,
) -> io::Result<()> {
    let seed = if write_position(meta, "snapshot")?.is_some() {
        let bytes = meta
            .get("compacted_snapshot")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing membership seed"))?;
        crate::consensus_storage::decode_owned_snapshot(bytes.value())
            .map_err(io::Error::other)?
            .0
            .membership
    } else {
        None
    };
    let first_replaced = replace_from.map(|offset| request.entries[offset].identity.index.0);
    let mut chain = Vec::new();
    for record in memberships.iter().map_err(io::Error::other)? {
        let (index, bytes) = record.map_err(io::Error::other)?;
        if first_replaced.is_none_or(|first| index.value() < first) {
            chain.push(stored_membership(bytes.value())?);
        }
    }
    if let Some(offset) = replace_from {
        for entry in &request.entries[offset..] {
            if let EntryPayload::Membership(membership) = &entry.payload {
                chain.push(crate::consensus_model::StoredMembership {
                    introduced_at: Some(entry.identity),
                    membership: membership.clone(),
                });
            }
        }
    }
    validate_owned_membership_chain(seed.as_ref(), &chain, confirmed).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid candidate membership chain",
        )
    })
}

fn validate_log_recovery(transaction: &redb::ReadTransaction) -> io::Result<()> {
    let meta = transaction.open_table(TABLE).map_err(io::Error::other)?;
    validate_read_install_receipts(&meta)?;
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
    let mut chain = Vec::new();
    let chain_seed = if seed.is_empty() {
        None
    } else {
        Some(stored_membership(&seed)?)
    };
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
            chain.push(stored_membership(bytes.value())?);
            latest_membership = bytes.value().to_vec();
            if applied.is_some_and(|applied| entry.identity.index <= applied.index) {
                latest_applied_membership.clone_from(&latest_membership);
            }
            membership_count += 1;
        }
        previous = Some(entry.identity);
    }
    validate_owned_membership_chain(chain_seed.as_ref(), &chain, commit)?;
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
    validate_recovery_coverage(&log, snapshot, commit, applied)
}

fn validate_recovery_coverage(
    log: &redb::ReadOnlyTable<u64, &[u8]>,
    snapshot: Option<LogIdentity>,
    commit: Option<LogIdentity>,
    applied: Option<LogIdentity>,
) -> io::Result<()> {
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

/// Execution-local evidence for election deadline scheduling, never a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElectionActivityKind {
    AuthenticatedLeader,
    GrantedVote,
}

/// Checked, incarnation-local activity; restart begins with no observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElectionActivity {
    pub sequence: u64,
    pub term: Term,
    pub peer: NodeId,
    pub kind: ElectionActivityKind,
    pub observed_at: std::time::Instant,
}

/// Coherent local durable positions and execution-local leadership/activity.
/// Diagnostic state, not a quorum-confirmed read or an authority lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedObservation {
    pub hard_state: HardState,
    pub local_leader: bool,
    pub election_activity: Option<ElectionActivity>,
    pub effective_membership: Option<crate::consensus_model::StoredMembership>,
    pub committed_membership: Option<crate::consensus_model::StoredMembership>,
    pub last_log: Option<LogIdentity>,
    pub committed: Option<LogIdentity>,
    pub applied: Option<LogIdentity>,
}

/// Canonical state captured under consumed quorum read authority. The watermark
/// is the round's committed position, not a lease or a reusable credential.
#[derive(Debug)]
pub struct AuthorizedRead {
    pub state: crate::control_state::ControlState,
    pub watermark: LogIdentity,
}

/// One durably applied entry; only application entries carry response bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedOutcome {
    pub identity: LogIdentity,
    pub response: Option<Vec<u8>>,
}

/// A bounded transaction result, published only after canonical state commits.
/// `more` means the committed watermark exceeds this resulting applied position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedBatch {
    pub outcomes: Vec<AppliedOutcome>,
    pub applied: Option<LogIdentity>,
    pub more: bool,
}

#[derive(Debug)]
pub enum ProposalError {
    InvalidCommand(String),
    Unauthorized(String),
    StaleAuthority,
    NotReady,
    IndexExhausted,
    /// Predictable retained-log exhaustion; compact applied state and retry.
    Capacity,
    Append(AppendExecutionError),
    Executor(VoteGateError),
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
    campaign: Option<crate::consensus_model::Campaign>,
    leader: Option<crate::consensus_model::LeaderProgress>,
    initialized_noop: Option<LogIdentity>,
    read_barrier: crate::consensus_model::ReadBarrier,
    read_context: Option<crate::consensus_model::ReadContext>,
    read_membership: Option<crate::consensus_model::StoredMembership>,
    election_activity: Option<ElectionActivity>,
    staging_expiry: Option<tokio::task::JoinHandle<()>>,
    staging: Option<(std::fs::File, crate::transfer_staging::TransferStaging)>,
}

impl DurableVoteGate {
    fn record_election_activity(
        &mut self,
        peer: NodeId,
        kind: ElectionActivityKind,
    ) -> Result<(), VoteGateError> {
        if self.election_activity.is_some_and(|activity| {
            activity.term == self.store.state.term
                && activity.kind == ElectionActivityKind::AuthenticatedLeader
                && activity.peer != peer
        }) {
            // Conflicting same-term claims cannot keep postponing an election.
            return Ok(());
        }
        let Some(sequence) = self
            .election_activity
            .map_or(Some(1), |activity| activity.sequence.checked_add(1))
        else {
            self.store.failed = true;
            return Err(VoteGateError::EffectExhausted);
        };
        self.election_activity = Some(ElectionActivity {
            sequence,
            term: self.store.state.term,
            peer,
            kind,
            observed_at: std::time::Instant::now(),
        });
        Ok(())
    }

    fn observe_verified_term(&mut self, peer: NodeId, term: Term) -> Result<bool, VoteGateError> {
        let (_, membership) = self.store.election_context()?;
        let eligible = membership.as_ref().is_some_and(|membership| {
            membership
                .configurations()
                .iter()
                .any(|set| set.contains(&peer))
        });
        if eligible && term > self.store.state.term {
            self.cancel_read();
            self.leader = None;
            self.campaign = None;
            self.initialized_noop = None;
            self.store
                .persist(HardState {
                    term,
                    voted_for: None,
                })
                .map_err(|_| VoteGateError::StorageFailure)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Called only after exact signed-response verification (or by unit tests
    /// exercising the deterministic adapter boundary).
    fn observe_read_confirmation(
        &mut self,
        peer: NodeId,
        response: crate::consensus_model::ReadConfirmation,
    ) -> Result<(), VoteGateError> {
        let observation = self.store.observe()?;
        let Some(membership) = observation.effective_membership else {
            self.cancel_read();
            return Ok(());
        };
        if let Some(term) = self.read_barrier.observe(
            self.leader.as_ref(),
            observation.hard_state,
            &membership,
            peer,
            response,
        ) {
            self.cancel_read();
            self.leader = None;
            self.campaign = None;
            self.initialized_noop = None;
            self.store
                .persist(HardState {
                    term,
                    voted_for: None,
                })
                .map_err(|_| VoteGateError::StorageFailure)?;
        }
        Ok(())
    }

    fn cancel_read(&mut self) {
        self.read_barrier.cancel();
        self.read_context = None;
        self.read_membership = None;
    }

    /// Construct only from recovered durable authority and a fresh incarnation.
    #[must_use]
    pub fn new(incarnation: u128, store: OwnedVoteStore) -> Self {
        let _ = incarnation;
        Self {
            #[cfg(test)]
            gate: VoteGate::new(incarnation, store.state()),
            store,
            campaign: None,
            leader: None,
            initialized_noop: None,
            read_barrier: crate::consensus_model::ReadBarrier::default(),
            read_context: None,
            read_membership: None,
            election_activity: None,
            staging_expiry: None,
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
    closing: std::sync::Arc<std::sync::atomic::AtomicBool>,
    validation: std::sync::Arc<tokio::sync::Semaphore>,
    outbound_votes: std::sync::Arc<tokio::sync::Semaphore>,
    response_pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    read_outbound: std::sync::Arc<tokio::sync::Semaphore>,
    status: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

/// Append admission errors are distinct from uncertain durable completion.
#[derive(Debug)]
pub enum AppendExecutionError {
    InvalidRequest(crate::consensus_model::AppendError),
    CompactionRequired,
    Executor(VoteGateError),
    Storage(io::Error),
}

const fn vote_rpc_error(
    error: VoteGateError,
) -> bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError {
    use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
    match error {
        VoteGateError::Busy => RpcError::CapacityUnavailable,
        VoteGateError::Stopped => RpcError::Stopped,
        VoteGateError::StorageFailure => RpcError::StorageFailure,
        VoteGateError::EffectExhausted => RpcError::ExecutionFailed,
    }
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
            let recovery_permit = permit;
            let store = match OwnedVoteStore::open(&path, &cluster) {
                Ok(store) => store,
                Err(error) => {
                    // A completed failed attempt must release admission before
                    // its caller can observe the error and retry recovery.
                    drop(recovery_permit);
                    let _ = send.send(Err(error));
                    return;
                }
            };
            let executor = Self::new(DurableVoteGate::new(incarnation, store));
            if let Err(error) = executor.handoff(send, acknowledged) {
                tracing::error!(%error, "cancelled owned recovery cleanup failed");
            }
            drop(recovery_permit);
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
            status: gate.store.status.clone(),
            gate: std::sync::Arc::new(tokio::sync::Mutex::new(Some(gate))),
            closing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            outbound_votes: std::sync::Arc::new(tokio::sync::Semaphore::new(16)),
            response_pending: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            read_outbound: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            validation: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
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
            gate.staging_expiry = Some(tokio::spawn(async move {
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
            }));
            Ok(Ok(()))
        })
        .await
        .map_err(|error| format!("staging configuration failed: {error:?}"))?
    }

    /// Authenticate staging mutations under single-flight authority. Canonical
    /// validation runs outside authority; sealing repeats authoritative admission.
    /// Offsets acknowledge staging, not replication.
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
        if operation != "finish" {
            return self
                .transfer_phase(caller, local, operation, request, None)
                .await
                .map(|(offset, _)| offset);
        }
        crate::consensus_network::transfer_signing_payload(&operation, &request)?;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("owned validation stopped".into());
        }
        let permit = self
            .validation
            .clone()
            .try_acquire_owned()
            .map_err(|_| "owned validation busy".to_string())?;
        let executor = Self {
            gate: self.gate.clone(),
            closing: self.closing.clone(),
            outbound_votes: self.outbound_votes.clone(),
            response_pending: self.response_pending.clone(),
            read_outbound: self.read_outbound.clone(),
            validation: self.validation.clone(),
            status: self.status.clone(),
        };
        // The task, not the request future, owns admission until all I/O and
        // publication attempts terminate. Shutdown waits on this same permit.
        tokio::spawn(async move {
            let (_, ticket) = executor
                .transfer_phase(
                    caller.clone(),
                    local,
                    operation.clone(),
                    request.clone(),
                    None,
                )
                .await?;
            let ticket = ticket.ok_or("missing prepared validation")?;
            let header = request.header.clone();
            let closing = executor.closing.clone();
            let status = executor.status.clone();
            let (validated, permit) = tokio::task::spawn_blocking(move || {
                // Carry admission through worker completion even if its async
                // supervisor is dropped. Return it with the validation outcome.
                let result = ticket.validate(|file, total| {
                    crate::consensus_network::validate_staged_object(
                        file, total, &header, &status, &closing,
                    )
                });
                (result, permit)
            })
            .await
            .map_err(|error| error.to_string())?;
            let validated = validated.map_err(|error| error.to_string())?;
            let result = executor
                .transfer_phase(caller, local, operation, request, Some(validated))
                .await
                .map(|(offset, _)| offset);
            drop(permit);
            result
        })
        .await
        .map_err(|error| error.to_string())?
    }

    #[allow(clippy::too_many_lines)] // Keep authentication and authoritative admission together.
    async fn transfer_phase<C: crate::ClusterRuntimeOps + Send + Sync + 'static>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        operation: String,
        request: bmux_cluster_plugin_api::cluster_consensus_transfer_types::ChunkRequest,
        validated: Option<crate::transfer_staging::ValidatedTicket>,
    ) -> Result<(u64, Option<crate::transfer_staging::ValidationTicket>), String> {
        // Charge and validate before moving attacker-controlled data into a task.
        crate::consensus_network::transfer_signing_payload(&operation, &request)?;
        let finishing = validated.is_some();
        self.run_admitted(finishing, move |gate| {
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
            Ok((|| -> Result<_, String> {
                let source = if finishing {
                    crate::membership::revalidate_peer_for_members(
                        caller.as_ref(),
                        &crate::ClusterPeerAuthenticateRequest {
                            proof: request.proof.peer.clone(),
                        },
                        &members,
                    )?
                    .node_id
                    .parse::<NodeId>()?
                } else {
                    crate::consensus_network::authenticate_transfer_request(
                        caller.as_ref(),
                        &operation,
                        local,
                        &members,
                        &request,
                    )?
                };
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
                    "finish" => {
                        if let Some(ticket) = validated {
                            staging
                                .complete_validation(key, ticket)
                                .map(|()| request.header.total_bytes)
                        } else {
                            return staging
                                .prepare_validation(key)
                                .map(|ticket| (request.header.total_bytes, Some(ticket)))
                                .map_err(|error| error.to_string());
                        }
                    }
                    "abort" => staging.abort(key).map(|()| 0),
                    _ => return Err("unknown transfer operation".into()),
                };
                result
                    .map(|offset| (offset, None))
                    .map_err(|error| error.to_string())
            })())
        })
        .await
        .map_err(|error| format!("owned transfer admission failed: {error:?}"))?
    }

    /// Authenticate and consume append entries under the single durable owner.
    ///
    /// Missing staging is an error, never a replication acknowledgment. This
    /// unpublished path does not yet resolve staged retries from the durable log.
    /// # Errors
    /// Rejects stale authority, invalid objects, missing staging and storage failure.
    pub async fn append_authenticated<C: crate::ClusterRuntimeOps + Send + Sync + 'static>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> Result<AppendDecision, AuthenticatedAppendError> {
        self.append_authenticated_version(caller, local, request, false)
            .await?
            .0
            .ok_or(AuthenticatedAppendError::TermMismatch)
    }

    /// V2 signs the exact request-bound durable outcome, including stale terms.
    /// # Errors
    /// Authentication or uncertain persistence never produces a signed outcome.
    pub async fn append_v2_authenticated<C: crate::ClusterRuntimeOps + Send + Sync + 'static>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> Result<
        bmux_cluster_plugin_api::cluster_consensus_rpc_v2_types::AppendResponse,
        AuthenticatedAppendError,
    > {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types::AppendOutcome as V1,
            cluster_consensus_rpc_v2_types::{AppendOutcome as V2, AppendResponse},
        };
        let identity = crate::load_or_create_node_identity(caller.as_ref())
            .map_err(|_| AuthenticatedAppendError::Unauthorized)?;
        if *identity.node_id() != local {
            return Err(AuthenticatedAppendError::Unauthorized);
        }
        let (outcome, term) = match self
            .append_authenticated_version(caller, local, request.clone(), true)
            .await
        {
            Ok((None, term)) => (V2::StaleTerm, term),
            Ok((Some(decision), term)) => {
                let value = match decision.outcome {
                    Ok(AppendOutcome::Reconciled(AppendReconciliation::Matched {
                        through,
                        ..
                    })) => V1::Durable {
                        matched: through.map(wire_log_identity),
                    },
                    Ok(AppendOutcome::Reconciled(AppendReconciliation::PreviousMismatch)) => {
                        V1::Conflict {
                            next_index: request
                                .previous
                                .as_ref()
                                .map_or(0, |previous| previous.index),
                        }
                    }
                    _ => {
                        return Err(AuthenticatedAppendError::Invalid(
                            "append reconciliation rejected".into(),
                        ));
                    }
                };
                (V2::Accepted { value }, term)
            }
            Err(AuthenticatedAppendError::MissingTransfer { reference, term }) => (
                V2::Accepted {
                    value: V1::MissingTransfer {
                        reference: *reference,
                    },
                },
                term,
            ),
            Err(error) => return Err(error),
        };
        let mut response = AppendResponse {
            request_id: request.request_id,
            source: local.as_bytes().to_vec(),
            term: term.0,
            outcome,
            signature: Vec::new(),
        };
        response.signature = identity.sign(
            &crate::consensus_network::owned_append_v2_response_signing_payload(
                &request, &response,
            )?,
        );
        Ok(response)
    }

    // Keep authentication, authoritative eligibility, term persistence and append
    // in one serialized admission; V1 and V2 differ only at explicit wire boundaries.
    #[allow(clippy::too_many_lines)]
    async fn append_authenticated_version<C: crate::ClusterRuntimeOps + Send + Sync + 'static>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
        v2: bool,
    ) -> Result<(Option<AppendDecision>, Term), AuthenticatedAppendError> {
        if v2 {
            crate::consensus_network::owned_append_v2_signing_payload(&request)?;
        } else {
            crate::consensus_network::owned_append_signing_payload(&request)?;
        }
        self.run(move |gate| {
            use bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendEntry;
            use bmux_cluster_plugin_api::cluster_consensus_transfer_types::{
                EntryPurpose, TransferHeader, TransferPurpose,
            };
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
                Ok((state, cluster.value().to_vec(), membership))
            })()
            .map_err(|_| VoteGateError::StorageFailure)?;
            store.failed = false;
            let outcome = (|| -> Result<_, AuthenticatedAppendError> {
                if request.cluster_id.as_bytes() != authority.1 {
                    return Err(AuthenticatedAppendError::Unauthorized);
                }
                let members = authority.0.members.into_values().collect::<Vec<_>>();
                let source = if v2 {
                    crate::consensus_network::authenticate_owned_append_v2(
                        caller.as_ref(),
                        local,
                        &members,
                        &request,
                    )
                } else {
                    crate::consensus_network::authenticate_owned_append(
                        caller.as_ref(),
                        local,
                        &members,
                        &request,
                    )
                }
                .map_err(|_| AuthenticatedAppendError::Unauthorized)?;
                if !authority.2.as_ref().is_some_and(|membership| {
                    membership.nodes().contains_key(&local)
                        && membership
                            .configurations()
                            .iter()
                            .any(|set| set.contains(&source))
                }) {
                    return Err(AuthenticatedAppendError::Unauthorized);
                }
                if request.term < store.state.term.0 {
                    return Ok((source, None, false, store.state.term));
                }
                if request.term > store.state.term.0 {
                    // Authentication and voter eligibility precede authority changes.
                    // Persist even if subsequent staging or predecessor lookup fails.
                    store
                        .persist(HardState {
                            term: Term(request.term),
                            voted_for: None,
                        })
                        .map_err(AuthenticatedAppendError::Storage)?;
                }
                gate.campaign = None;
                gate.leader = None;
                let mut previous = request.previous.clone();
                let mut entries = Vec::with_capacity(request.entries.len());
                let mut remaining = 32 * 1024 * 1024usize;
                for entry in &request.entries {
                    let retained = if let AppendEntry::Staged { reference } = entry {
                        store
                            .retained_transfer_entry(reference, remaining)
                            .map_err(|error| {
                                if store.failed {
                                    AuthenticatedAppendError::Storage(error)
                                } else {
                                    AuthenticatedAppendError::Invalid(error.to_string())
                                }
                            })?
                    } else {
                        None
                    };
                    let canonical = if let Some(bytes) = retained {
                        bytes
                    } else {
                        match entry {
                            AppendEntry::Inline { canonical } => canonical.clone(),
                            AppendEntry::Staged { reference } => {
                                let length = usize::try_from(reference.total_bytes)
                                    .map_err(|_| "entry length overflow")?;
                                if length > remaining {
                                    return Err("append canonical byte budget exceeded".into());
                                }
                                let header = TransferHeader {
                                    cluster_id: request.cluster_id.clone(),
                                    sender: request.sender.clone(),
                                    recipient: request.recipient.clone(),
                                    leader_term: request.term,
                                    sender_incarnation: reference.sender_incarnation,
                                    sequence: reference.sequence,
                                    purpose: TransferPurpose::Entry {
                                        value: EntryPurpose {
                                            identity: reference.identity.clone(),
                                            previous: previous.clone(),
                                        },
                                    },
                                    total_bytes: reference.total_bytes,
                                    checksum: reference.checksum.clone(),
                                };
                                let binding = Sha256::digest(
                                    bmux_plugin_sdk::encode_service_message(&header)
                                        .map_err(|error| error.to_string())?,
                                )
                                .into();
                                let key = crate::transfer_staging::TransferKey {
                                    peer: *source.as_bytes(),
                                    incarnation: reference.sender_incarnation,
                                    sequence: reference.sequence,
                                };
                                let (_, staging) =
                                    gate.staging.as_mut().ok_or("staging is not configured")?;
                                staging
                                    .expire(std::time::Instant::now())
                                    .map_err(AuthenticatedAppendError::Storage)?;
                                staging.validate_binding(key, binding).map_err(
                                    |error| match error.kind() {
                                        io::ErrorKind::NotFound => {
                                            AuthenticatedAppendError::MissingTransfer {
                                                reference: Box::new(reference.clone()),
                                                term: store.state.term,
                                            }
                                        }
                                        io::ErrorKind::InvalidInput => {
                                            AuthenticatedAppendError::Invalid(error.to_string())
                                        }
                                        _ => AuthenticatedAppendError::Storage(error),
                                    },
                                )?;
                                let mut bytes = vec![0; length];
                                let mut offset = 0;
                                while offset < length {
                                    if store.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
                                        return Err(AuthenticatedAppendError::Executor(
                                            VoteGateError::Stopped,
                                        ));
                                    }
                                    let end = length.min(offset + 64 * 1024);
                                    let read = staging
                                        .read(key, offset as u64, &mut bytes[offset..end])
                                        .map_err(AuthenticatedAppendError::Storage)?;
                                    if read == 0 {
                                        return Err("incomplete staged entry".into());
                                    }
                                    offset += read;
                                }
                                bytes
                            }
                        }
                    };
                    remaining = remaining
                        .checked_sub(canonical.len())
                        .ok_or("append canonical byte budget exceeded")?;
                    let decoded = crate::consensus_storage::decode_owned_entry(&canonical)
                        .map_err(|error| error.to_string())?;
                    if let AppendEntry::Staged { reference } = entry
                        && decoded.identity
                            != crate::consensus_network::transfer_position(&reference.identity)?
                    {
                        return Err("staged entry identity mismatch".into());
                    }
                    previous = Some(
                        bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity {
                            term: decoded.identity.term.0,
                            leader: decoded.identity.leader.as_bytes().to_vec(),
                            index: decoded.identity.index.0,
                        },
                    );
                    entries.push(decoded);
                }
                let decoded = AppendRequest {
                    term: Term(request.term),
                    leader: source,
                    previous: request
                        .previous
                        .as_ref()
                        .map(crate::consensus_network::transfer_position)
                        .transpose()?,
                    entries,
                    leader_commit: request
                        .leader_commit
                        .as_ref()
                        .map(crate::consensus_network::transfer_position)
                        .transpose()?,
                };
                // Authentication/eligible higher-term persistence precedes this
                // structural check. Malformed peer input is not storage failure.
                decoded.validate(64, 32 * 1024 * 1024).map_err(|error| {
                    AuthenticatedAppendError::Invalid(format!("invalid append request: {error:?}"))
                })?;
                let protocol_valid = store
                    .append_activity_valid(&decoded)
                    .map_err(AuthenticatedAppendError::Storage)?;
                let decision = store
                    .append(&decoded, (64, 32 * 1024 * 1024))
                    .map_err(AuthenticatedAppendError::Storage)?;
                let leader_activity = protocol_valid
                    && decision.term == store.state.term
                    && matches!(decision.outcome, Ok(AppendOutcome::Reconciled(_)));
                Ok((source, Some(decision), leader_activity, store.state.term))
            })();
            Ok(match outcome {
                Ok((source, decision, leader_activity, term)) => {
                    // A healthy prefix mismatch is normal during follower repair.
                    // Invalid membership, equivocation and storage failure are not.
                    if leader_activity {
                        gate.record_election_activity(
                            source,
                            ElectionActivityKind::AuthenticatedLeader,
                        )?;
                    }
                    Ok((decision, term))
                }
                Err(error) => Err(error),
            })
        })
        .await
        .map_err(AuthenticatedAppendError::Executor)?
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
            let result = gate.store.append(&request, limits);
            if result.as_ref().is_ok_and(|decision| decision.step_down) {
                gate.campaign = None;
                gate.leader = None;
            }
            Ok(result)
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

    /// Authenticate and persist a vote under the single durable authority owner.
    /// # Errors
    /// Authentication and storage failures never produce a granted response.
    pub async fn vote_authenticated<C: crate::ClusterRuntimeOps + Send + Sync + 'static>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
    ) -> Result<VoteResponse, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        crate::consensus_network::owned_vote_signing_payload(&request)
            .map_err(|_| RpcError::InvalidRequest)?;
        self.run(move |gate| {
            let store = &mut gate.store;
            let _ = store.election_context()?;
            store.failed = true;
            let authority = (|| -> io::Result<_> {
                let tx = store.database.begin_read().map_err(io::Error::other)?;
                let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
                let bytes = meta
                    .get("control")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing control authority"))?;
                let state = crate::control_state::ControlState::decode_snapshot(bytes.value())
                    .map_err(io::Error::other)?;
                let cluster = meta
                    .get("cluster")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing cluster authority"))?;
                Ok((state, cluster.value().to_vec()))
            })()
            .map_err(|_| VoteGateError::StorageFailure)?;
            store.failed = false;
            let outcome = (|| -> Result<_, RpcError> {
                if request.cluster_id.as_bytes() != authority.1 {
                    return Err(RpcError::Unauthorized);
                }
                let members = authority.0.members.into_values().collect::<Vec<_>>();
                let source = crate::consensus_network::authenticate_owned_vote(
                    caller.as_ref(),
                    local,
                    &members,
                    &request,
                )
                .map_err(|_| RpcError::Unauthorized)?;
                let response = store
                    .vote(
                        local,
                        RequestVote {
                            term: Term(request.term),
                            candidate: source,
                            last_log: request
                                .last_log
                                .as_ref()
                                .map(crate::consensus_network::transfer_position)
                                .transpose()
                                .map_err(|_| RpcError::InvalidRequest)?,
                        },
                    )
                    .map_err(vote_rpc_error)?;
                Ok((source, response))
            })();
            Ok(match outcome {
                Ok((source, response)) => {
                    if response.granted {
                        gate.record_election_activity(source, ElectionActivityKind::GrantedVote)?;
                    }
                    if gate
                        .campaign
                        .as_ref()
                        .is_some_and(|campaign| campaign.request.term != response.term)
                    {
                        gate.campaign = None;
                    }
                    Ok(response)
                }
                Err(error) => Err(error),
            })
        })
        .await
        .map_err(vote_rpc_error)?
    }

    /// Campaign only if the deadline's complete authority observation still holds.
    /// Comparison, voter eligibility and durable self-vote share one admission;
    /// a watch notification is not authority to campaign.
    /// # Errors
    /// Returns busy/stopped, exhausted identities or uncertain persistence errors.
    pub async fn campaign_if_unchanged(
        &self,
        local: NodeId,
        expected_hard_state: HardState,
        expected_membership: crate::consensus_model::StoredMembership,
        expected_activity_sequence: Option<u64>,
    ) -> Result<Option<RequestVote>, VoteGateError> {
        self.run(move |gate| {
            let observation = gate.store.observe()?;
            let eligible_leader = gate.leader.as_ref().is_some_and(|leader| {
                leader.term == observation.hard_state.term
                    && observation.hard_state.voted_for == Some(local)
                    && leader.membership == expected_membership.membership
                    && leader.matched.contains_key(&local)
            });
            if eligible_leader
                || observation.hard_state != expected_hard_state
                || observation.effective_membership.as_ref() != Some(&expected_membership)
                || gate.election_activity.map(|activity| activity.sequence)
                    != expected_activity_sequence
                || !expected_membership
                    .membership
                    .configurations()
                    .iter()
                    .any(|voters| voters.contains(&local))
            {
                return Ok(None);
            }
            let request = gate.store.campaign(local)?;
            if let Some(request) = request {
                gate.cancel_read();
                gate.leader = None;
                gate.initialized_noop = None;
                gate.campaign = Some(crate::consensus_model::Campaign::new(
                    request,
                    expected_membership.membership,
                ));
            }
            Ok(request)
        })
        .await
    }

    /// Begin an eligible local campaign using authoritative log and membership.
    /// The returned request may be sent only after the self-vote has committed.
    /// This does not establish leadership; the caller must collect a current quorum.
    /// # Errors
    /// Busy, stopped, exhausted terms, and uncertain persistence suppress requests.
    pub async fn campaign(&self, local: NodeId) -> Result<Option<RequestVote>, VoteGateError> {
        self.run(move |gate| {
            gate.campaign = None;
            gate.leader = None;
            let request = gate.store.campaign(local)?;
            if let Some(request) = request {
                let (_, membership) = gate.store.election_context()?;
                let membership = membership.ok_or(VoteGateError::Stopped)?;
                gate.campaign = Some(crate::consensus_model::Campaign::new(request, membership));
            }
            Ok(request)
        })
        .await
    }

    /// Send one locally signed vote request and verify its bound response before
    /// observing election authority. Endpoint and negotiated interfaces are resolved
    /// from durable control membership under the authority executor.
    /// # Errors
    /// Transport failure or timeout has no vote outcome; callers must not count it.
    pub async fn request_campaign_vote<C: bmux_plugin::ServiceCaller + Send + Sync + 'static>(
        &self,
        caller: &C,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
    ) -> Result<bool, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::Stopped);
        }
        let outbound = self
            .outbound_votes
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::CapacityUnavailable)?;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::Stopped);
        }
        let signed = crate::consensus_network::owned_vote_signing_payload(&request)
            .map_err(|_| RpcError::InvalidRequest)?;
        if request.sender.as_slice() != local.as_bytes() || request.proof.signature.len() != 64 {
            return Err(RpcError::Unauthorized);
        }
        crate::membership::verify_node_signature(
            &local.to_string(),
            &signed,
            &request.proof.signature,
        )
        .map_err(|_| RpcError::Unauthorized)?;
        let recipient = NodeId::from_bytes(
            request
                .recipient
                .as_slice()
                .try_into()
                .map_err(|_| RpcError::InvalidRequest)?,
        );
        let cluster = request.cluster_id.clone();
        let term = Term(request.term);
        let endpoint = self
            .prepare_outbound(&outbound, move |gate| {
                if !gate.campaign.as_ref().is_some_and(|campaign| {
                    campaign.request.term == term
                        && campaign.request.candidate == local
                        && campaign
                            .membership
                            .configurations()
                            .iter()
                            .any(|voters| voters.contains(&recipient))
                }) {
                    return Ok(Err(RpcError::Unauthorized));
                }
                gate.store.failed = true;
                let endpoint = (|| -> io::Result<_> {
                    let tx = gate.store.database.begin_read().map_err(io::Error::other)?;
                    let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
                    let bytes = meta
                        .get("control")
                        .map_err(io::Error::other)?
                        .ok_or_else(|| invalid("missing control authority"))?;
                    let state = crate::control_state::ControlState::decode_snapshot(bytes.value())
                        .map_err(io::Error::other)?;
                    Ok(owned_vote_endpoint(&state, &cluster, recipient))
                })()
                .map_err(|_| VoteGateError::StorageFailure)?;
                gate.store.failed = false;
                Ok(endpoint)
            })
            .await
            .map_err(vote_rpc_error)??;
        let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bmux_cluster_plugin_api::cluster_consensus_rpc::client::vote(
                &mut remote,
                request.clone(),
            ),
        )
        .await
        .map_err(|_| RpcError::ExecutionFailed)?
        .map_err(|_| RpcError::ExecutionFailed)??;
        self.campaign_signed_response_admitted(local, &request, &response, outbound)
            .await
    }

    /// Drive at most eight probes in one leadership epoch, yielding between
    /// attempts. Errors stop the turn; the lifecycle owner schedules later turns.
    /// Cancellation drops the current probe without recording a match.
    /// # Errors
    /// Returns authority, authentication, capacity, transport or storage errors;
    /// staged-transfer requirements remain explicit rather than being retried.
    pub async fn replicate_turn<C>(
        &self,
        caller: &C,
        peer: NodeId,
    ) -> Result<ReplicationTurn, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
    {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        let (term, membership, _) = self.replication_status(peer).await?;
        for _ in 0..8 {
            let (current_term, current_membership, _) = self.replication_status(peer).await?;
            if current_term != term || current_membership != membership {
                return Err(RpcError::StaleTerm);
            }
            // Send even when previously caught up to propagate the commit watermark.
            self.replicate_next(caller, peer).await?;
            let (current_term, current_membership, caught_up) =
                self.replication_status(peer).await?;
            if current_term != term || current_membership != membership {
                return Err(RpcError::StaleTerm);
            }
            if caught_up {
                return Ok(ReplicationTurn::CaughtUp);
            }
            tokio::task::yield_now().await;
        }
        Ok(ReplicationTurn::Yielded)
    }

    async fn replication_status(
        &self,
        peer: NodeId,
    ) -> Result<
        (Term, Membership, bool),
        bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError,
    > {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        self.run(move |gate| {
            let (last, membership) = gate.store.election_context()?;
            let Some(leader) = gate.leader.as_ref().filter(|leader| {
                leader.term == gate.store.state.term
                    && membership.as_ref() == Some(&leader.membership)
            }) else {
                return Ok(Err(RpcError::StaleTerm));
            };
            let Some(matched) = leader.matched.get(&peer) else {
                return Ok(Err(RpcError::Unauthorized));
            };
            Ok(Ok((
                leader.term,
                leader.membership.clone(),
                last.is_some() && *matched == last,
            )))
        })
        .await
        .map_err(vote_rpc_error)?
    }

    /// Build and send one bounded probe from the current retry position.
    /// Each call obtains a fresh challenge; no credentials are retained in progress.
    /// # Errors
    /// Returns authority, authentication, transport or storage failures. Incompatible
    /// means the next entry/history requires the staged transfer path.
    pub async fn replicate_next<C>(
        &self,
        caller: &C,
        peer: NodeId,
    ) -> Result<(), bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
    {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types::{AppendEntry, AppendRequest as WireRequest, RpcError},
            cluster_consensus_transfer_types::TransferProof,
        };
        let preparation = self
            .outbound_votes
            .try_acquire()
            .map_err(|_| RpcError::CapacityUnavailable)?;
        let identity =
            crate::load_or_create_node_identity(caller).map_err(|_| RpcError::Unauthorized)?;
        let local = *identity.node_id();
        let batch = self
            .run(move |gate| gate.store.outbound_batch(gate.leader.as_ref(), local, peer))
            .await
            .map_err(vote_rpc_error)??;
        if !batch.v2 {
            return Err(RpcError::Incompatible);
        }
        let proof = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::endpoint::peer_authentication_proof(
                caller,
                &batch.endpoint,
                &local.to_string(),
                &peer.to_string(),
            ),
        )
        .await
        .map_err(|_| RpcError::ExecutionFailed)?
        .map_err(|_| RpcError::Unauthorized)?;
        let mut request = WireRequest {
            cluster_id: batch.cluster,
            sender: local.as_bytes().to_vec(),
            recipient: peer.as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4(),
            term: batch.append.term.0,
            previous: batch.append.previous.map(wire_log_identity),
            entries: batch
                .append
                .entries
                .iter()
                .map(|entry| {
                    crate::consensus_storage::encode_owned_entry(entry)
                        .map(|canonical| AppendEntry::Inline { canonical })
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| RpcError::StorageFailure)?,
            leader_commit: batch.append.leader_commit.map(wire_log_identity),
            proof: TransferProof {
                credential_serial: batch.credential_serial,
                signature: Vec::new(),
                peer: proof,
            },
        };
        let signed = crate::consensus_network::owned_append_v2_signing_payload(&request)
            .map_err(|_| RpcError::InvalidRequest)?;
        request.proof.signature = identity.sign(&signed);
        drop(preparation);
        self.replicate_append_v2(caller, local, request).await
    }

    /// Return the current retry position only while durable leader authority
    /// still matches. The transport uses this to construct its next signed probe.
    /// # Errors
    /// Returns executor shutdown, admission, or authoritative storage errors.
    pub async fn replication_next(
        &self,
        peer: NodeId,
    ) -> Result<Option<crate::consensus_model::LogIndex>, VoteGateError> {
        self.run(move |gate| {
            let (_, membership) = gate.store.election_context()?;
            Ok(gate
                .leader
                .as_ref()
                .filter(|leader| {
                    leader.term == gate.store.state.term
                        && membership.as_ref() == Some(&leader.membership)
                })
                .and_then(|leader| leader.next.get(&peer).copied()))
        })
        .await
    }

    /// Send explicitly negotiated V2; transport errors never trigger V1 fallback.
    /// # Errors
    /// Rejects missing selection, stale authority, invalid bindings or storage failure.
    pub async fn replicate_append_v2<C: bmux_plugin::ServiceCaller + Send + Sync + 'static>(
        &self,
        caller: &C,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> Result<(), bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types::{AppendResponse, RpcError},
            cluster_consensus_rpc_v2_types::AppendOutcome as V2,
        };
        let outbound = self
            .outbound_votes
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::CapacityUnavailable)?;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::Stopped);
        }
        let signed = crate::consensus_network::owned_append_v2_signing_payload(&request)
            .map_err(|_| RpcError::InvalidRequest)?;
        if request.sender.as_slice() != local.as_bytes() || request.proof.signature.len() != 64 {
            return Err(RpcError::Unauthorized);
        }
        crate::membership::verify_node_signature(
            &local.to_string(),
            &signed,
            &request.proof.signature,
        )
        .map_err(|_| RpcError::Unauthorized)?;
        let admitted = request.clone();
        let (endpoint, peer, expected, membership) = self
            .run(move |gate| {
                let admission =
                    gate.store
                        .admit_replication(gate.leader.as_ref(), local, &admitted)?;
                if let Ok((_, peer, _, _)) = &admission
                    && !gate.store.peer_supports_append_v2(*peer)?
                {
                    return Ok(Err(RpcError::Incompatible));
                }
                Ok(admission)
            })
            .await
            .map_err(vote_rpc_error)??;
        let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint.clone());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bmux_cluster_plugin_api::cluster_consensus_rpc_v2::client::append(
                &mut remote,
                request.clone(),
            ),
        )
        .await
        .map_err(|_| RpcError::ExecutionFailed)?
        .map_err(|_| RpcError::ExecutionFailed)??;
        crate::consensus_network::verify_owned_append_v2_response(&request, &response)
            .map_err(|_| RpcError::Unauthorized)?;
        let outcome = match response.outcome {
            V2::Accepted { value } => value,
            V2::StaleTerm => {
                return self
                    .settle_response(outbound, move |gate| {
                        if !gate.store.peer_supports_append_v2(peer)? {
                            return Ok(Err(RpcError::Unauthorized));
                        }
                        gate.observe_verified_term(peer, Term(response.term))?;
                        Ok(Err(RpcError::StaleTerm))
                    })
                    .await
                    .map_err(vote_rpc_error)?;
            }
        };
        self.replicate_response_admitted(
            local,
            request,
            AppendResponse {
                request_id: response.request_id,
                source: response.source,
                term: response.term,
                outcome,
                signature: response.signature,
            },
            endpoint,
            peer,
            expected,
            membership,
            outbound,
            true,
        )
        .await
    }

    /// Dispatch a bounded signed append from current leader authority. The exact
    /// request remains owned until its response is verified; cancellation drops
    /// the call without counting replication. Success is progress, not commit.
    /// # Errors
    /// Rejects incompatible peers, stale authority, invalid log claims and replies.
    pub async fn replicate_append<C: bmux_plugin::ServiceCaller + Send + Sync + 'static>(
        &self,
        caller: &C,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> Result<(), bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        let outbound = self
            .outbound_votes
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::CapacityUnavailable)?;
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RpcError::Stopped);
        }
        verify_outbound_append(local, &request)?;
        let admitted = request.clone();
        let (endpoint, peer, expected, membership) = self
            .run(move |gate| {
                gate.store
                    .admit_replication(gate.leader.as_ref(), local, &admitted)
            })
            .await
            .map_err(vote_rpc_error)??;
        let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint.clone());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bmux_cluster_plugin_api::cluster_consensus_rpc::client::append(
                &mut remote,
                request.clone(),
            ),
        )
        .await
        .map_err(|_| RpcError::ExecutionFailed)?
        .map_err(|_| RpcError::ExecutionFailed)??;
        crate::consensus_network::verify_owned_append_response(&request, &response)
            .map_err(|_| RpcError::Unauthorized)?;
        self.replicate_response_admitted(
            local, request, response, endpoint, peer, expected, membership, outbound, false,
        )
        .await
    }

    // The exact response has passed signature verification before this boundary.
    // Keep higher-term persistence ahead of every old-probe eligibility check.
    #[allow(clippy::too_many_arguments)]
    async fn replicate_response_admitted(
        &self,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
        response: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendResponse,
        endpoint: String,
        peer: NodeId,
        expected: Option<LogIdentity>,
        membership: Membership,
        outbound: tokio::sync::OwnedSemaphorePermit,
        v2: bool,
    ) -> Result<(), bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::{
            AppendOutcome as WireOutcome, RpcError,
        };
        self.settle_response(outbound, move |gate| {
            if v2 && !gate.store.peer_supports_append_v2(peer)? {
                return Ok(Err(RpcError::Unauthorized));
            }
            if gate.observe_verified_term(peer, Term(response.term))? {
                return Ok(Err(RpcError::StaleTerm));
            }
            let admission = gate
                .store
                .admit_replication(gate.leader.as_ref(), local, &request)?;
            let Ok((current_endpoint, _, _, current_membership)) = admission else {
                return Ok(Err(RpcError::StaleTerm));
            };
            if current_endpoint != endpoint || current_membership != membership {
                return Ok(Err(RpcError::Unauthorized));
            }

            if response.term != request.term {
                return Ok(Err(RpcError::StaleTerm));
            }
            if let WireOutcome::Durable { matched } = response.outcome {
                let Ok(matched) = matched
                    .as_ref()
                    .map(decode_replication_identity)
                    .transpose()
                else {
                    return Ok(Err(RpcError::InvalidRequest));
                };
                if matched != expected {
                    return Ok(Err(RpcError::InvalidRequest));
                }
                if let Some(leader) = gate.leader.as_mut() {
                    leader.acknowledge_probe(peer, matched);
                }
                if let Some(leader) = gate.leader.as_ref() {
                    gate.store.commit_replicated(leader)?;
                }
            } else if let WireOutcome::Conflict { next_index } = response.outcome {
                let previous = request
                    .previous
                    .as_ref()
                    .map(|id| crate::consensus_model::LogIndex(id.index));
                if let Some(leader) = gate.leader.as_mut() {
                    leader.reject_probe(
                        peer,
                        previous,
                        crate::consensus_model::LogIndex(next_index),
                    );
                }
            }
            Ok(Ok(()))
        })
        .await
        .map_err(vote_rpc_error)?
    }

    /// Verify a response to a locally issued signed request before observing its
    /// term or counting its vote. The caller retains the exact issued request;
    /// remote callers must not choose the request used for this binding.
    /// # Errors
    /// Invalid signatures/bindings cannot mutate election authority.
    pub async fn campaign_signed_response(
        &self,
        local: NodeId,
        request: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
        response: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteResponse,
    ) -> Result<bool, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        let permit = self
            .outbound_votes
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError::CapacityUnavailable
            })?;
        self.campaign_signed_response_admitted(local, request, response, permit)
            .await
    }

    async fn campaign_signed_response_admitted(
        &self,
        local: NodeId,
        request: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest,
        response: &bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteResponse,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<bool, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError> {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        if request.sender.as_slice() != local.as_bytes() || response.signature.len() != 64 {
            return Err(RpcError::Unauthorized);
        }
        let signed =
            crate::consensus_network::owned_vote_response_signing_payload(request, response)
                .map_err(|_| RpcError::InvalidRequest)?;
        let peer = NodeId::from_bytes(
            response
                .source
                .as_slice()
                .try_into()
                .map_err(|_| RpcError::InvalidRequest)?,
        );
        crate::membership::verify_node_signature(&peer.to_string(), &signed, &response.signature)
            .map_err(|_| RpcError::Unauthorized)?;
        self.campaign_response_admitted(
            Term(request.term),
            peer,
            VoteResponse {
                term: Term(response.term),
                granted: response.granted,
            },
            permit,
        )
        .await
        .map_err(vote_rpc_error)
    }

    /// Count a response from an authenticated peer for the specified campaign.
    /// A true result is election quorum evidence, not a linearizable read barrier.
    /// The transport owner must authenticate the peer and bind the response to
    /// this campaign before calling; this method is not a network admission API.
    /// # Errors
    /// Storage failure suppresses quorum evidence; admission is single-flight.
    pub async fn campaign_response(
        &self,
        campaign_term: Term,
        peer: NodeId,
        response: VoteResponse,
    ) -> Result<bool, VoteGateError> {
        let permit = self
            .outbound_votes
            .clone()
            .try_acquire_owned()
            .map_err(|_| VoteGateError::Busy)?;
        self.campaign_response_admitted(campaign_term, peer, response, permit)
            .await
    }

    async fn campaign_response_admitted(
        &self,
        campaign_term: Term,
        peer: NodeId,
        response: VoteResponse,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<bool, VoteGateError> {
        self.settle_response(permit, move |gate| {
            if gate.observe_verified_term(peer, response.term)? {
                return Ok(false);
            }
            let (_, membership) = gate.store.election_context()?;
            let decision = crate::consensus_model::Campaign::observe(
                &mut gate.campaign,
                gate.store.state,
                membership.as_ref(),
                campaign_term,
                peer,
                response,
            );
            if let Some(hard_state) = decision.persist {
                gate.store
                    .persist(hard_state)
                    .map_err(|_| VoteGateError::StorageFailure)?;
            }
            Ok(decision.quorum)
        })
        .await
    }

    /// Persist the elected leader's initial no-op before exposing it for replication.
    /// Consumes the campaign only after durable append; does not advance commit.
    /// # Errors
    /// Busy/stopped storage, exhausted indexes, or failed persistence suppress output.
    pub async fn initialize_leader(&self) -> Result<Option<AppendRequest>, VoteGateError> {
        self.run(|gate| {
            let (last_log, membership) = gate.store.election_context()?;
            let (Some(campaign), Some(membership)) = (&gate.campaign, membership) else {
                return Ok(None);
            };
            let Some(request) = campaign
                .leader_initialization(gate.store.state, &membership, last_log)
                .map_err(|_| VoteGateError::EffectExhausted)?
            else {
                return Ok(None);
            };
            if !gate.store.has_local_append_capacity(1024)? {
                return Err(VoteGateError::Busy);
            }
            let decision = gate.store.append(&request, (1, 1024)).map_err(|error| {
                if error.kind() == io::ErrorKind::WouldBlock {
                    VoteGateError::Busy
                } else {
                    VoteGateError::StorageFailure
                }
            })?;
            if !matches!(
                decision.outcome,
                Ok(AppendOutcome::Reconciled(
                    AppendReconciliation::Matched { .. }
                ))
            ) {
                gate.campaign = None;
                return Err(VoteGateError::Stopped);
            }
            gate.leader = Some(
                crate::consensus_model::LeaderProgress::after_durable_initialization(
                    membership,
                    request.entries[0].identity,
                ),
            );
            gate.initialized_noop = Some(request.entries[0].identity);
            gate.campaign = None;
            Ok(Some(request))
        })
        .await
    }

    /// Begin one fresh round, superseding any prior unfinished context.
    /// # Errors
    /// Busy/stopped storage or exhausted context sequence.
    pub async fn begin_read(
        &self,
        local: NodeId,
    ) -> Result<Option<crate::consensus_model::ReadContext>, VoteGateError> {
        self.run(move |gate| {
            gate.read_barrier.cancel();
            gate.read_context = None;
            gate.read_membership = None;
            let observation = gate.store.observe()?;
            let (Some(leader), Some(membership)) =
                (gate.leader.as_ref(), observation.effective_membership)
            else {
                return Ok(None);
            };
            if observation.hard_state.voted_for != Some(local) {
                return Ok(None);
            }
            let context = gate
                .read_barrier
                .begin(
                    leader,
                    observation.hard_state,
                    &membership,
                    observation.committed,
                )
                .map_err(|_| VoteGateError::EffectExhausted)?;
            gate.read_context = context;
            gate.read_membership = context.map(|_| membership);
            Ok(context)
        })
        .await
    }

    /// Consume a confirmed round once and capture canonical state under the
    /// SAME serialized authority scope. Product authorization remains required.
    /// # Errors
    /// Busy/stopped admission or canonical storage/decode failure.
    pub async fn complete_read(
        &self,
        local: NodeId,
        context: crate::consensus_model::ReadContext,
    ) -> Result<Option<AuthorizedRead>, AppendExecutionError> {
        self.run(move |gate| {
            if gate.read_context != Some(context) || context.leader != local {
                return Ok(Ok(None));
            }
            let observation = gate.store.observe()?;
            let Some(membership) = observation.effective_membership else {
                gate.cancel_read();
                return Ok(Ok(None));
            };
            let watermark = gate.read_barrier.complete(
                gate.leader.as_ref(),
                observation.hard_state,
                &membership,
                observation.committed,
                observation.applied,
            );
            let Some(watermark) = watermark else {
                return Ok(Ok(None));
            };
            gate.cancel_read();
            Ok(gate
                .store
                .read_control_state()
                .map(|state| Some(AuthorizedRead { state, watermark })))
        })
        .await
        .map_err(AppendExecutionError::Executor)?
        .map_err(AppendExecutionError::Storage)
    }

    /// Confirm a round through one fresh authenticated signed append heartbeat.
    /// Exact request UUID/body remain local until verification; no context is
    /// trusted from the wire. Uses the shared bounded outbound task permit.
    /// # Errors
    /// Returns explicit stale authority, authentication, timeout or storage errors.
    // Keep request binding, authentication and durable higher-term handling auditable.
    #[allow(clippy::too_many_lines)]
    pub async fn confirm_read<C>(
        &self,
        caller: &C,
        context: crate::consensus_model::ReadContext,
        peer: NodeId,
    ) -> Result<(), bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError>
    where
        C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
    {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types::{
                AppendOutcome as WireOutcome, AppendRequest as WireRequest, RpcError,
            },
            cluster_consensus_transfer_types::TransferProof,
        };
        let read_permit = self
            .read_outbound
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::CapacityUnavailable)?;
        let permit = self
            .outbound_votes
            .clone()
            .try_acquire_owned()
            .map_err(|_| RpcError::CapacityUnavailable)?;
        let identity =
            crate::load_or_create_node_identity(caller).map_err(|_| RpcError::Unauthorized)?;
        let local = *identity.node_id();
        if local != context.leader || local == peer {
            return Err(RpcError::Unauthorized);
        }
        let batch = self
            .run(move |gate| {
                if gate.read_context != Some(context) {
                    return Ok(Err(RpcError::StaleTerm));
                }
                // Reuse canonical endpoint/member admission but send no entries.
                gate.store
                    .outbound_batch_inner(gate.leader.as_ref(), local, peer, true)
            })
            .await
            .map_err(vote_rpc_error)??;
        let proof = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::endpoint::peer_authentication_proof(
                caller,
                &batch.endpoint,
                &local.to_string(),
                &peer.to_string(),
            ),
        )
        .await
        .map_err(|_| RpcError::ExecutionFailed)?
        .map_err(|_| RpcError::Unauthorized)?;
        let mut request = WireRequest {
            cluster_id: batch.cluster,
            sender: local.as_bytes().to_vec(),
            recipient: peer.as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4(),
            term: context.term.0,
            previous: batch.append.previous.map(wire_log_identity),
            entries: Vec::new(),
            leader_commit: batch.append.leader_commit.map(wire_log_identity),
            proof: TransferProof {
                credential_serial: batch.credential_serial,
                signature: Vec::new(),
                peer: proof,
            },
        };
        request.proof.signature = identity.sign(
            &crate::consensus_network::owned_append_signing_payload(&request)
                .map_err(|_| RpcError::InvalidRequest)?,
        );
        let admitted = request.clone();
        let (endpoint, _, expected, membership) = self
            .run(move |gate| {
                if gate.read_context != Some(context) {
                    return Ok(Err(RpcError::StaleTerm));
                }
                gate.store
                    .admit_replication(gate.leader.as_ref(), local, &admitted)
            })
            .await
            .map_err(vote_rpc_error)??;
        let mut remote = crate::endpoint::EndpointDispatchClient::new(caller, endpoint.clone());
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bmux_cluster_plugin_api::cluster_consensus_rpc::client::append(
                &mut remote,
                request.clone(),
            ),
        )
        .await
        .map_err(|_| RpcError::ExecutionFailed)?
        .map_err(|_| RpcError::ExecutionFailed)??;
        crate::consensus_network::verify_owned_append_response(&request, &response)
            .map_err(|_| RpcError::Unauthorized)?;
        self.settle_response(permit, move |gate| {
            let _read_permit = read_permit;
            // Process a verified eligible-peer higher term even when this round
            // was superseded while I/O was in flight. Never revive its authority.
            let observation = gate.store.observe()?;
            let Some(current_membership) = observation.effective_membership else { gate.cancel_read(); return Ok(Err(RpcError::StaleTerm)); };
            let state = gate.store.read_control_state().map_err(|_| VoteGateError::StorageFailure)?;
            let current_endpoint = match owned_vote_endpoint(&state, &request.cluster_id, peer) {
                Ok(endpoint) => endpoint,
                Err(error) => return Ok(Err(error)),
            };
            if current_endpoint != endpoint { return Ok(Err(RpcError::Unauthorized)); }
            let eligible = current_membership.membership.configurations().iter().any(|set| set.contains(&peer));
            if eligible && response.term > observation.hard_state.term.0 {
                gate.cancel_read(); gate.leader = None; gate.campaign = None; gate.initialized_noop = None;
                gate.store.persist(HardState { term: Term(response.term), voted_for: None }).map_err(|_| VoteGateError::StorageFailure)?;
                return Ok(Err(RpcError::StaleTerm));
            }
            if gate.read_context != Some(context) || current_membership.membership != membership { return Ok(Err(RpcError::StaleTerm)); }
            let admission = gate.store.admit_replication(gate.leader.as_ref(), local, &request)?;
            let Ok((current_endpoint, _, _, _)) = admission else { return Ok(Err(RpcError::StaleTerm)); };
            if current_endpoint != endpoint { return Ok(Err(RpcError::Unauthorized)); }
            let accepted = matches!(response.outcome, WireOutcome::Durable { matched } if matched == expected.map(wire_log_identity));
            gate.observe_read_confirmation(peer, crate::consensus_model::ReadConfirmation { context, term: Term(response.term), accepted })?;
            Ok(Ok(()))
        }).await.map_err(vote_rpc_error)?
    }

    /// Read coherent local positions; this does not establish a read barrier.
    /// # Errors
    /// Rejects busy/stopped admission or invalid durable state.
    pub async fn observe(&self, local: NodeId) -> Result<OwnedObservation, VoteGateError> {
        self.run(move |gate| {
            let mut observation = gate.store.observe()?;
            observation.election_activity = gate.election_activity;
            observation.local_leader = gate.leader.as_ref().is_some_and(|leader| {
                leader.term == observation.hard_state.term
                    && observation.hard_state.voted_for == Some(local)
                    && observation
                        .effective_membership
                        .as_ref()
                        .is_some_and(|m| m.membership == leader.membership)
                    && leader.matched.contains_key(&local)
            });
            Ok(observation)
        })
        .await
    }

    /// Read canonical applied state, not a cache. Linearizable callers must
    /// separately establish a quorum barrier and wait for its applied watermark.
    /// # Errors
    /// Rejects busy/stopped admission or canonical storage/decode failure.
    pub async fn read_control_state(
        &self,
    ) -> Result<crate::control_state::ControlState, AppendExecutionError> {
        self.run(|gate| Ok(gate.store.read_control_state()))
            .await
            .map_err(AppendExecutionError::Executor)?
            .map_err(AppendExecutionError::Storage)
    }

    /// Admit an authenticated local command. The caller supplies the actual
    /// authenticated principal, NEVER a principal copied from untrusted input.
    /// `authorize` must revalidate product/resource permissions against canonical
    /// state inside this admission. It must be bounded, nonblocking and without
    /// side effects. Codec/principal validation is not product authorization.
    /// Success means durable local append, NOT quorum commit or application.
    /// Retry ambiguous completion with the same command identity for dedup.
    /// # Errors
    /// Rejects invalid allocation, denied authorization, stale authority, absent
    /// current-term committed no-op, unapplied committed work, capacity pressure,
    /// or uncertain persistence. `Capacity` is retryable after compaction; it
    /// never represents an uncertain storage outcome.
    // Keep authority admission, authorization and durable self-match publication
    // in one auditable single-flight operation.
    #[allow(clippy::too_many_lines)]
    pub async fn propose_authorized<F>(
        &self,
        local: NodeId,
        expected: HardState,
        authenticated_principal: String,
        command: bmux_cluster_plugin_api::cluster_types::ControlCommand,
        authorize: F,
    ) -> Result<LogIdentity, ProposalError>
    where
        F: FnOnce(
                &crate::control_state::ControlState,
                &bmux_cluster_plugin_api::cluster_types::ControlCommand,
            ) -> Result<(), String>
            + Send
            + 'static,
    {
        crate::control_codec::control_command_encoded_len(&command, 1_048_576)
            .map_err(|error| ProposalError::InvalidCommand(error.to_string()))?;
        if command.schema_version != 1 {
            return Err(ProposalError::InvalidCommand(
                "unsupported control command schema".into(),
            ));
        }
        if authenticated_principal.is_empty() || command.principal_id != authenticated_principal {
            return Err(ProposalError::Unauthorized(
                "authenticated principal mismatch".into(),
            ));
        }
        self.run(move |gate| {
            if gate.store.state != expected {
                return Ok(Err(ProposalError::StaleAuthority));
            }
            let observation = gate.store.observe()?;
            let eligible = gate.leader.as_ref().is_some_and(|leader| {
                leader.term == expected.term
                    && expected.voted_for == Some(local)
                    && observation
                        .effective_membership
                        .as_ref()
                        .is_some_and(|m| m.membership == leader.membership)
                    && leader.matched.contains_key(&local)
            });
            let ready = gate.initialized_noop.is_some_and(|noop| {
                noop.term == expected.term
                    && noop.leader == local
                    && crate::consensus_model::validate_position_prefix(
                        Some(noop),
                        observation.committed,
                    )
                    .is_ok()
            });
            if !eligible || !ready || observation.applied != observation.committed {
                return Ok(Err(ProposalError::NotReady));
            }
            let state = match gate.store.read_control_state() {
                Ok(state) => state,
                Err(error) => {
                    return Ok(Err(ProposalError::Append(AppendExecutionError::Storage(
                        error,
                    ))));
                }
            };
            if let Err(error) = authorize(&state, &command) {
                return Ok(Err(ProposalError::Unauthorized(error)));
            }
            let Some(index) = observation
                .last_log
                .map_or(Some(0), |last| last.index.0.checked_add(1))
            else {
                return Ok(Err(ProposalError::IndexExhausted));
            };
            let identity = LogIdentity {
                term: expected.term,
                leader: local,
                index: crate::consensus_model::LogIndex(index),
            };
            let entry = Entry {
                identity,
                payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                    &command,
                )),
            };
            let canonical = crate::consensus_storage::encode_owned_entry(&entry)
                .map_err(|_| VoteGateError::StorageFailure)?;
            if canonical.len() > APPLICATION_ENTRY_BUDGET {
                return Ok(Err(ProposalError::InvalidCommand(
                    "framed entry exceeds application budget".into(),
                )));
            }
            if !gate.store.has_local_append_capacity(canonical.len())? {
                return Ok(Err(ProposalError::Capacity));
            }
            let request = AppendRequest {
                term: expected.term,
                leader: local,
                previous: observation.last_log,
                entries: vec![entry],
                leader_commit: observation.committed,
            };
            match gate.store.append(&request, (1, 1_048_576)) {
                Ok(decision)
                    if matches!(
                        decision.outcome,
                        Ok(AppendOutcome::Reconciled(
                            AppendReconciliation::Matched { .. }
                        ))
                    ) =>
                {
                    if let Some(leader) = gate.leader.as_mut() {
                        leader.acknowledge_probe(local, Some(identity));
                    }
                    Ok(Ok(identity))
                }
                Ok(_) => {
                    gate.store.failed = true;
                    Err(VoteGateError::Stopped)
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    Ok(Err(ProposalError::Capacity))
                }
                Err(error) => Ok(Err(ProposalError::Append(AppendExecutionError::Storage(
                    error,
                )))),
            }
        })
        .await
        .map_err(ProposalError::Executor)?
    }

    /// Advance commit from current retained replication evidence without applying
    /// commands. This also handles a single voter, which needs no remote reply.
    /// # Errors
    /// Uncertain storage stops the owner and suppresses commit success.
    pub async fn commit_replicated(&self) -> Result<Option<LogIdentity>, VoteGateError> {
        self.run(|gate| {
            let Some(leader) = gate.leader.as_ref() else {
                return Ok(None);
            };
            gate.store.commit_replicated(leader)
        })
        .await
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

    /// Apply one identity-tagged bounded batch under the same single-flight
    /// owner as append, votes and canonical reads. Responses and the resulting
    /// watermark are released only after the canonical transaction commits.
    /// # Errors
    /// Busy/stopped admission or uncertain decode/storage suppresses the batch.
    pub async fn apply_committed_batch(&self) -> Result<AppliedBatch, AppendExecutionError> {
        self.run(|gate| Ok(gate.store.apply_committed_batch()))
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

    /// Authenticate and consume an exact sealed snapshot under canonical owner
    /// authority. Replay receipts survive staging loss/restart and are retained
    /// for the last 64 successful installs. Every retry authenticates again.
    /// # Errors
    /// Authentication, executor and uncertain storage errors never claim install.
    // Keep authentication, replay lookup, sealing and atomic persistence together.
    #[allow(clippy::too_many_lines)]
    pub async fn install_snapshot_authenticated<C>(
        &self,
        caller: std::sync::Arc<C>,
        local: NodeId,
        request: bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
    ) -> Result<
        bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallResponse,
        bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallError,
    >
    where
        C: crate::ClusterRuntimeOps + Send + Sync + 'static,
    {
        use bmux_cluster_plugin_api::cluster_consensus_snapshot_types::{
            InstallError, InstallOutcome, InstallRejection, InstallResponse,
        };
        use bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferPurpose;
        crate::consensus_network::owned_snapshot_install_signing_payload(&request)
            .map_err(|_| InstallError::InvalidRequest)?;
        self.run(move |gate| {
            let outcome = (|| -> Result<_, InstallError> {
                let state = gate
                    .store
                    .read_control_state()
                    .map_err(|_| InstallError::StorageFailure)?;
                if state.cluster_id != request.reference.cluster_id {
                    return Err(InstallError::Unauthorized);
                }
                let members = state.members.into_values().collect::<Vec<_>>();
                let source = crate::consensus_network::authenticate_owned_snapshot_install(
                    caller.as_ref(),
                    local,
                    &members,
                    &request,
                )
                .map_err(|_| InstallError::Unauthorized)?;
                if !members.iter().any(|member| {
                    member.node_id == local.to_string()
                        && member.state
                            == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
                        && crate::consensus_network::supports_owned_snapshot_install(member)
                }) {
                    return Err(InstallError::Incompatible);
                }
                let observation = gate
                    .store
                    .observe()
                    .map_err(|_| InstallError::StorageFailure)?;
                if observation.applied != observation.committed {
                    // Canonical product membership may lag committed admission
                    // or revocation intent. Apply first; never authenticate from
                    // a projection known to trail the committed watermark.
                    return Err(InstallError::CapacityUnavailable);
                }
                let eligible =
                    observation
                        .effective_membership
                        .as_ref()
                        .is_some_and(|membership| {
                            membership.membership.nodes().contains_key(&local)
                                && membership
                                    .membership
                                    .configurations()
                                    .iter()
                                    .any(|set| set.contains(&source))
                        });
                if !eligible {
                    return Err(InstallError::Unauthorized);
                }
                let identity = crate::load_or_create_node_identity(caller.as_ref())
                    .map_err(|_| InstallError::Unauthorized)?;
                if *identity.node_id() != local {
                    return Err(InstallError::Unauthorized);
                }
                let TransferPurpose::Snapshot { value } = &request.reference.purpose else {
                    return Err(InstallError::InvalidRequest);
                };
                let boundary = value
                    .last_included
                    .as_ref()
                    .ok_or(InstallError::InvalidRequest)?;
                let canonical_boundary = decode_replication_identity(boundary)
                    .map_err(|_| InstallError::InvalidRequest)?;
                let binding: [u8; 32] = Sha256::digest(
                    bmux_plugin_sdk::encode_service_message(&request.reference)
                        .map_err(|_| InstallError::InvalidRequest)?,
                )
                .into();
                let key = crate::transfer_staging::TransferKey {
                    peer: *source.as_bytes(),
                    incarnation: request.reference.sender_incarnation,
                    sequence: request.reference.sequence,
                };
                let mut receipt = [0; INSTALL_RECEIPT_BYTES];
                receipt[..16].copy_from_slice(request.request_id.as_bytes());
                receipt[16..48].copy_from_slice(source.as_bytes());
                receipt[48..64].copy_from_slice(key.incarnation.as_bytes());
                receipt[64..72].copy_from_slice(&key.sequence.to_le_bytes());
                receipt[72..104].copy_from_slice(&binding);
                receipt[104..152].copy_from_slice(&encode_position(Some(canonical_boundary)));
                receipt[152..160].copy_from_slice(&request.reference.leader_term.to_le_bytes());
                gate.observe_verified_term(source, Term(request.reference.leader_term))
                    .map_err(|_| InstallError::StorageFailure)?;
                let replay = gate
                    .store
                    .install_replay(&receipt)
                    .map_err(|_| InstallError::StorageFailure)?;
                let result = if let Some(matches) = replay {
                    if matches {
                        InstallOutcome::Installed {
                            boundary: boundary.clone(),
                        }
                    } else {
                        InstallOutcome::Rejected {
                            reason: InstallRejection::ConflictingReference,
                        }
                    }
                } else if request.reference.leader_term < observation.hard_state.term.0 {
                    InstallOutcome::Rejected {
                        reason: InstallRejection::StaleTerm,
                    }
                } else {
                    if request.reference.leader_term > gate.store.state.term.0 {
                        gate.store
                            .persist(HardState {
                                term: Term(request.reference.leader_term),
                                voted_for: None,
                            })
                            .map_err(|_| InstallError::StorageFailure)?;
                    }
                    gate.leader = None;
                    gate.campaign = None;
                    gate.initialized_noop = None;
                    gate.cancel_read();
                    if snapshot_install_fits(0, request.reference.total_bytes, 0, 0) {
                        let bytes = (|| -> Result<Vec<u8>, InstallRejection> {
                            let (_, staging) = gate
                                .staging
                                .as_mut()
                                .ok_or(InstallRejection::UnknownTransfer)?;
                            let (file, total) =
                                staging
                                    .sealed_reader(key, binding)
                                    .map_err(|error| match error.kind() {
                                        io::ErrorKind::NotFound => {
                                            InstallRejection::UnknownTransfer
                                        }
                                        io::ErrorKind::WouldBlock => InstallRejection::NotSealed,
                                        _ => InstallRejection::ConflictingReference,
                                    })?;
                            if total != request.reference.total_bytes {
                                return Err(InstallRejection::ConflictingReference);
                            }
                            // This is bounded whole-object decoding, NOT streaming:
                            // cap permits <4 MiB input; canonical decoder/control
                            // validation and transaction copies may hold ~8 copies
                            // concurrently (<32 MiB plus bounded metadata/state).
                            // No second unbounded staging Vec or peer-sized task.
                            let mut bytes = vec![
                                0;
                                usize::try_from(total).map_err(|_| {
                                    InstallRejection::CapacityUnavailable
                                })?
                            ];
                            file.read_exact(&mut bytes)
                                .map_err(|_| InstallRejection::InvalidSnapshot)?;
                            if Sha256::digest(&bytes).as_slice() != request.reference.checksum {
                                return Err(InstallRejection::InvalidSnapshot);
                            }
                            crate::consensus_network::validate_staged_object(
                                file,
                                total,
                                &request.reference,
                                &gate.store.status,
                                &std::sync::atomic::AtomicBool::new(false),
                            )
                            .map_err(|_| InstallRejection::InvalidSnapshot)?;
                            Ok(bytes)
                        })();
                        match bytes {
                            Err(reason) => InstallOutcome::Rejected { reason },
                            Ok(bytes) => match gate.store.install_snapshot_recorded(
                                gate.store.state,
                                &bytes,
                                Some(&receipt),
                            ) {
                                Ok(()) => {
                                    // Cleanup cannot downgrade durable install.
                                    // Failed deletion retains staging reservation;
                                    // the durable receipt remains retry authority.
                                    if let Some((_, staging)) = gate.staging.as_mut() {
                                        let _ = staging.abort(key);
                                    }
                                    if gate.store.state.term == Term(request.reference.leader_term)
                                    {
                                        gate.record_election_activity(
                                            source,
                                            ElectionActivityKind::AuthenticatedLeader,
                                        )
                                        .map_err(|_| InstallError::ExecutionFailed)?;
                                    }
                                    InstallOutcome::Installed {
                                        boundary: boundary.clone(),
                                    }
                                }
                                Err(error) if !gate.store.failed => InstallOutcome::Rejected {
                                    reason: if error.kind() == io::ErrorKind::StorageFull {
                                        InstallRejection::CapacityUnavailable
                                    } else {
                                        InstallRejection::InvalidSnapshot
                                    },
                                },
                                Err(_) => return Err(InstallError::StorageFailure),
                            },
                        }
                    } else {
                        InstallOutcome::Rejected {
                            reason: InstallRejection::CapacityUnavailable,
                        }
                    }
                };
                // A receipt labels the original durable term, even if local
                // authority advanced since install. This preserves exact replay.
                let term = if matches!(result, InstallOutcome::Installed { .. }) {
                    request.reference.leader_term
                } else {
                    gate.store.state.term.0
                };
                let mut response = InstallResponse {
                    request_id: request.request_id,
                    source: local.as_bytes().to_vec(),
                    term,
                    outcome: result,
                    signature: Vec::new(),
                };
                response.signature = identity.sign(
                    &crate::consensus_network::owned_snapshot_install_response_signing_payload(
                        &request, &response,
                    )
                    .map_err(|_| InstallError::ExecutionFailed)?,
                );
                Ok(response)
            })();
            Ok(outcome)
        })
        .await
        .map_err(|error| match error {
            VoteGateError::Busy => InstallError::CapacityUnavailable,
            VoteGateError::StorageFailure => InstallError::StorageFailure,
            VoteGateError::Stopped => InstallError::Stopped,
            VoteGateError::EffectExhausted => InstallError::ExecutionFailed,
        })?
    }

    /// Install canonical snapshot bytes under the single-flight storage owner.
    ///
    /// This is a local persistence primitive, NOT a remote RPC or proof of peer
    /// authorization. A future authenticated install boundary must consume a
    /// sealed snapshot-purpose transfer and revalidate its complete immutable
    /// header before calling this. Transfer `finish` must never call this API.
    /// `expected` fences work prepared against superseded term/vote authority.
    /// # Errors
    /// Rejects stale authority, unsupported/corrupt snapshots and rollback of
    /// acknowledged commit. Storage uncertainty permanently stops this owner.
    pub async fn install_snapshot(
        &self,
        expected: HardState,
        canonical: Vec<u8>,
    ) -> Result<(), AppendExecutionError> {
        if !snapshot_install_fits(0, canonical.len() as u64, 0, 0) {
            return Err(AppendExecutionError::Storage(io::Error::new(
                io::ErrorKind::StorageFull,
                "snapshot exceeds owned install capacity",
            )));
        }
        self.run(move |gate| {
            let result = gate.store.install_snapshot(expected, &canonical);
            if result.is_ok() {
                gate.campaign = None;
                gate.leader = None;
                gate.initialized_noop = None;
                gate.cancel_read();
            }
            Ok(result)
        })
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

    /// Verified peer authority must settle even if its caller is cancelled or
    /// ordinary single-flight admission is busy. Fence subsequent admission
    /// synchronously, then retain one bounded outbound slot through settlement.
    async fn settle_response<T: Send + 'static>(
        &self,
        permit: tokio::sync::OwnedSemaphorePermit,
        operation: impl FnOnce(&mut DurableVoteGate) -> Result<T, VoteGateError> + Send + 'static,
    ) -> Result<T, VoteGateError> {
        self.response_pending
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pending = self.response_pending.clone();
        let owner = self.gate.clone();
        tokio::spawn(async move {
            let guard = owner.lock_owned().await;
            tokio::task::spawn_blocking(move || {
                struct Settlement(std::sync::Arc<std::sync::atomic::AtomicUsize>);
                impl Drop for Settlement {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let _settlement = Settlement(pending);
                let _permit = permit;
                let mut guard = guard;
                let mut gate = guard.take().ok_or(VoteGateError::Stopped)?;
                let result = operation(&mut gate);
                *guard = Some(gate);
                result
            })
            .await
            .map_err(|_| VoteGateError::Stopped)?
        })
        .await
        .map_err(|_| VoteGateError::Stopped)?
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut DurableVoteGate) -> Result<T, VoteGateError> + Send + 'static,
    ) -> Result<T, VoteGateError> {
        self.run_admitted(false, operation).await
    }

    /// FIFO preparation is available only to work already holding a bounded
    /// outbound slot. At most 16 such preparations can wait; cancellation removes
    /// the lock waiter. No peer I/O occurs under the gate. Response settlement
    /// fences still take precedence and return Busy rather than stale authority.
    async fn prepare_outbound<T: Send + 'static>(
        &self,
        _permit: &tokio::sync::OwnedSemaphorePermit,
        operation: impl FnOnce(&mut DurableVoteGate) -> Result<T, VoteGateError> + Send + 'static,
    ) -> Result<T, VoteGateError> {
        self.run_admitted(true, operation).await
    }

    // Only workflows already holding validation or outbound permits may wait.
    // Ordinary requests retain immediate Busy admission with no queue.
    async fn run_admitted<T: Send + 'static>(
        &self,
        finishing: bool,
        operation: impl FnOnce(&mut DurableVoteGate) -> Result<T, VoteGateError> + Send + 'static,
    ) -> Result<T, VoteGateError> {
        if self
            .response_pending
            .load(std::sync::atomic::Ordering::SeqCst)
            != 0
        {
            return Err(VoteGateError::Busy);
        }
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(VoteGateError::Stopped);
        }
        let mut guard = if finishing {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                self.gate.clone().lock_owned(),
            )
            .await
            .map_err(|_| VoteGateError::Busy)?
        } else {
            self.gate
                .clone()
                .try_lock_owned()
                .map_err(|_| VoteGateError::Busy)?
        };
        if self
            .response_pending
            .load(std::sync::atomic::Ordering::SeqCst)
            != 0
        {
            return Err(VoteGateError::Busy);
        }
        if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(VoteGateError::Stopped);
        }
        tokio::task::spawn_blocking(move || {
            let mut gate = guard.take().ok_or(VoteGateError::Stopped)?;
            let mut response = operation(&mut gate);
            if gate.leader.is_some() {
                match gate.store.election_context() {
                    Ok((_, membership))
                        if gate.leader.as_ref().is_some_and(|leader| {
                            leader.term == gate.store.state.term
                                && membership.as_ref() == Some(&leader.membership)
                        }) => {}
                    Ok(_) => gate.leader = None,
                    Err(error) => {
                        gate.leader = None;
                        response = Err(error);
                    }
                }
            }
            if gate.read_context.is_some() {
                match gate.store.observe() {
                    Ok(observation)
                        if gate.leader.is_some()
                            && observation.effective_membership == gate.read_membership
                            && gate.read_context.is_some_and(|context| {
                                context.term == observation.hard_state.term
                                    && Some(context.leader) == observation.hard_state.voted_for
                            }) => {}
                    Ok(_) => gate.cancel_read(),
                    Err(error) => {
                        gate.cancel_read();
                        response = Err(error);
                    }
                }
            }
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
    // The guard must span expiry joining and move into the blocking close task;
    // dropping it earlier permits cleanup to race canonical-store retirement.
    #[allow(clippy::significant_drop_tightening)]
    pub async fn shutdown(&self) -> Result<(), VoteGateError> {
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _outbound = self
            .outbound_votes
            .acquire_many(16)
            .await
            .map_err(|_| VoteGateError::Stopped)?;
        let _validation = self
            .validation
            .acquire()
            .await
            .map_err(|_| VoteGateError::Stopped)?;
        let mut guard = self.gate.clone().lock_owned().await;
        // Taking the gate drains any already admitted expiry worker before abort.
        if let Some(task) = guard.as_mut().and_then(|gate| gate.staging_expiry.take()) {
            task.abort();
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                return Err(VoteGateError::Stopped);
            }
        }
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

/// Sealed snapshot bytes staged from a leader, found by sender and boundary.
pub type StagedSnapshot = Box<dyn FnMut(NodeId, LogIdentity) -> io::Result<Vec<u8>> + Send>;

/// The owned store as the consensus driver's durable authority.
pub struct OwnedStorage {
    store: OwnedVoteStore,
    staged: StagedSnapshot,
}

impl OwnedStorage {
    #[must_use]
    pub fn new(store: OwnedVoteStore, staged: StagedSnapshot) -> Self {
        Self { store, staged }
    }
}

impl crate::consensus_driver::Storage for OwnedStorage {
    type View = crate::control_state::ControlState;

    fn write(&mut self, write: &crate::consensus_engine::Write) -> io::Result<()> {
        self.store.write(write)
    }

    fn install_snapshot(
        &mut self,
        from: NodeId,
        last_included: LogIdentity,
        membership: &crate::consensus_model::StoredMembership,
    ) -> io::Result<()> {
        let bytes = (self.staged)(from, last_included)?;
        let (descriptor, _) =
            crate::consensus_storage::decode_owned_snapshot(&bytes).map_err(io::Error::other)?;
        // The staged object must be exactly the snapshot the engine accepted.
        if descriptor.last_included != Some(last_included)
            || descriptor.membership.as_ref() != Some(membership)
        {
            return Err(invalid(
                "staged snapshot does not match the accepted boundary",
            ));
        }
        let expected = self.store.state;
        self.store.install_snapshot(expected, &bytes)
    }

    fn apply(&mut self, through: LogIdentity) -> io::Result<Vec<(LogIdentity, Vec<u8>)>> {
        let mut responses = Vec::new();
        loop {
            let batch = self.store.apply_committed_batch()?;
            responses.extend(batch.outcomes.into_iter().filter_map(|outcome| {
                outcome
                    .response
                    .map(|response| (outcome.identity, response))
            }));
            let reached = batch
                .applied
                .is_some_and(|applied| applied.index >= through.index);
            if reached || !batch.more {
                if !reached {
                    return Err(invalid("application stopped before the durable commit"));
                }
                return Ok(responses);
            }
        }
    }

    fn compact(
        &mut self,
    ) -> io::Result<Option<(LogIdentity, crate::consensus_model::StoredMembership)>> {
        let bytes = self.store.publish_snapshot()?;
        let (descriptor, _) =
            crate::consensus_storage::decode_owned_snapshot(&bytes).map_err(io::Error::other)?;
        let (Some(boundary), Some(membership)) = (descriptor.last_included, descriptor.membership)
        else {
            return Ok(None);
        };
        self.store.compact_snapshot()?;
        Ok(Some((boundary, membership)))
    }

    fn read(&mut self) -> io::Result<Self::View> {
        self.store.read_control_state()
    }

    fn close(self) -> io::Result<()> {
        self.store.close()
    }
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
        let genesis = encode_owned_genesis(genesis)?;
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
            table
                .insert("install_receipts", &[][..])
                .map_err(io::Error::other)?;
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

    // Keep the immediate transaction and its rejection/poisoning boundary
    // together so partial authority publication remains auditable.
    #[allow(clippy::too_many_lines)]
    fn install_replay(
        &mut self,
        candidate: &[u8; INSTALL_RECEIPT_BYTES],
    ) -> io::Result<Option<bool>> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let tx = self.database.begin_read().map_err(io::Error::other)?;
        let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
        let receipts = meta
            .get("install_receipts")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing install receipts"))?;
        validate_read_install_receipts(&meta)?;
        let result = receipts
            .value()
            .as_chunks::<INSTALL_RECEIPT_BYTES>()
            .0
            .iter()
            .find(|old| old[..48] == candidate[..48] || old[16..72] == candidate[16..72])
            .map(|old| old.as_slice() == candidate);
        self.failed = false;
        Ok(result)
    }

    fn install_snapshot(&mut self, expected: HardState, bytes: &[u8]) -> io::Result<()> {
        self.install_snapshot_recorded(expected, bytes, None)
    }

    // Canonical state and the optional authenticated install receipt share ONE
    // immediate transaction. Local snapshot callers use the same implementation.
    #[allow(clippy::too_many_lines)]
    fn install_snapshot_recorded(
        &mut self,
        expected: HardState,
        bytes: &[u8],
        receipt: Option<&[u8; INSTALL_RECEIPT_BYTES]>,
    ) -> io::Result<()> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        if self.state != expected {
            return Err(invalid("snapshot prepared against stale hard state"));
        }
        if !snapshot_install_fits(0, bytes.len() as u64, 0, 0) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "snapshot exceeds owned install capacity",
            ));
        }
        // Decode with the canonical codec, including all control/dedup/workflow
        // state. Never reconstruct an application projection from a descriptor.
        let (descriptor, control) =
            crate::consensus_storage::decode_owned_snapshot(bytes).map_err(io::Error::other)?;
        let boundary = descriptor
            .last_included
            .ok_or_else(|| invalid("empty snapshot"))?;
        if boundary.term > expected.term {
            return Err(invalid("snapshot exceeds current term"));
        }
        let membership = descriptor
            .membership
            .ok_or_else(|| invalid("missing snapshot membership"))?;
        let introduced = membership
            .introduced_at
            .ok_or_else(|| invalid("unpositioned snapshot membership"))?;
        let membership_bytes = crate::consensus_storage::encode_owned_entry(&Entry {
            identity: membership
                .introduced_at
                .ok_or_else(|| invalid("unpositioned snapshot membership"))?,
            payload: EntryPayload::Membership(membership.membership),
        })
        .map_err(io::Error::other)?;
        let membership_size = u64::try_from(membership_bytes.len()).map_err(io::Error::other)?;
        // Reject obvious oversize before even opening a write transaction.
        if !snapshot_install_fits(0, bytes.len() as u64, membership_size, 0) {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "snapshot exceeds owned install capacity",
            ));
        }
        self.failed = true;
        let mut tx = self.database.begin_write().map_err(io::Error::other)?;
        tx.set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        let stats = tx.stats().map_err(io::Error::other)?;
        let existing = stats
            .allocated_pages()
            .checked_mul(stats.page_size() as u64)
            .and_then(|bytes| bytes.checked_add(stats.fragmented_bytes()))
            .ok_or_else(|| invalid("install capacity accounting overflow"))?;
        {
            let mut meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let get = |key| -> io::Result<Vec<u8>> {
                Ok(meta
                    .get(key)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing snapshot install authority"))?
                    .value()
                    .to_vec())
            };
            let committed = decode_position(&get("commit")?)?;
            let applied = decode_position(&get("applied")?)?;
            let snapshot = decode_position(&get("snapshot")?)?;
            // An equal term has exactly one leader throughout its prefix, even
            // when the incoming boundary is not present in the retained log.
            if [committed, applied, snapshot]
                .into_iter()
                .flatten()
                .any(|known| known.term == boundary.term && known.leader != boundary.leader)
                || (introduced.term == boundary.term && introduced.leader != boundary.leader)
            {
                self.failed = false;
                return Err(invalid("snapshot contradicts equal-term leader authority"));
            }
            // Membership identities fix their payload, independently of whether
            // the snapshot boundary matches a local log entry. Include applied
            // authority and the compacted prefix, not just retained entries.
            let mut known_memberships = vec![get("applied_membership")?];
            if snapshot.is_some() {
                let (old, _) =
                    crate::consensus_storage::decode_owned_snapshot(&get("compacted_snapshot")?)
                        .map_err(io::Error::other)?;
                let old = old
                    .membership
                    .ok_or_else(|| invalid("missing compacted membership"))?;
                known_memberships.push(
                    crate::consensus_storage::encode_owned_entry(&Entry {
                        identity: old
                            .introduced_at
                            .ok_or_else(|| invalid("unpositioned compacted membership"))?,
                        payload: EntryPayload::Membership(old.membership),
                    })
                    .map_err(io::Error::other)?,
                );
            }
            {
                let memberships = tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
                if let Some(committed) = committed {
                    for record in memberships
                        .range(..=committed.index.0)
                        .map_err(io::Error::other)?
                    {
                        known_memberships
                            .push(record.map_err(io::Error::other)?.1.value().to_vec());
                    }
                }
            }
            for bytes in known_memberships.iter().filter(|bytes| !bytes.is_empty()) {
                let known = crate::consensus_storage::decode_owned_entry(bytes)
                    .map_err(io::Error::other)?;
                if known.identity.index > boundary.index {
                    // A later committed membership may survive in a matching
                    // suffix; the commit-conflict check below proves coverage.
                    continue;
                }
                if (known.identity.term == boundary.term
                    && known.identity.leader != boundary.leader)
                    || (known.identity.index == introduced.index
                        && (known.identity != introduced || bytes != &membership_bytes))
                    || (known.identity.index > introduced.index)
                    || crate::consensus_model::validate_position_prefix(
                        Some(known.identity),
                        Some(introduced),
                    )
                    .is_err()
                    || (known.identity.term == introduced.term
                        && known.identity.leader != introduced.leader)
                {
                    self.failed = false;
                    return Err(invalid(
                        "snapshot contradicts authoritative membership history",
                    ));
                }
            }
            // A request rejection is not uncertain storage. Leave the owner
            // usable, with the uncommitted write transaction dropped unchanged.
            if descriptor.cluster_id.as_bytes() != get("cluster")?
                || crate::consensus_model::validate_position_prefix(applied, Some(boundary))
                    .is_err()
            {
                self.failed = false;
                return Err(invalid("snapshot cluster mismatch or applied rollback"));
            }
            let matches = if snapshot == Some(boundary) {
                true
            } else {
                let log = tx.open_table(LOG).map_err(io::Error::other)?;
                log.get(boundary.index.0)
                    .map_err(io::Error::other)?
                    .map(|entry| crate::consensus_storage::decode_owned_entry(entry.value()))
                    .transpose()
                    .map_err(io::Error::other)?
                    .is_some_and(|entry| entry.identity == boundary)
            };
            // Preserve committed suffixes only when the exact full boundary
            // identity matches; index equality alone never licenses truncation.
            if committed.is_some_and(|c| c.index >= boundary.index)
                && (!matches
                    || crate::consensus_model::validate_position_prefix(Some(boundary), committed)
                        .is_err())
            {
                self.failed = false;
                return Err(invalid("snapshot conflicts with acknowledged commit"));
            }
            if committed.is_some_and(|c| c.index < boundary.index)
                && crate::consensus_model::validate_position_prefix(committed, Some(boundary))
                    .is_err()
            {
                self.failed = false;
                return Err(invalid("snapshot term regresses committed prefix"));
            }
            if applied == Some(boundary)
                && (get("control")? != control || get("applied_membership")? != membership_bytes)
            {
                self.failed = false;
                return Err(invalid("snapshot conflicts with applied canonical state"));
            }
            let candidate_commit = if matches && committed.is_some_and(|c| c.index > boundary.index)
            {
                committed
            } else {
                Some(boundary)
            };
            let incoming_membership = stored_membership(&membership_bytes)?;
            let mut retained_memberships = Vec::new();
            if matches {
                let table = tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
                for record in table
                    .range((
                        std::ops::Bound::Excluded(boundary.index.0),
                        std::ops::Bound::Unbounded,
                    ))
                    .map_err(io::Error::other)?
                {
                    retained_memberships.push(stored_membership(
                        record.map_err(io::Error::other)?.1.value(),
                    )?);
                }
            }
            if validate_owned_membership_chain(
                Some(&incoming_membership),
                &retained_memberships,
                candidate_commit,
            )
            .is_err()
            {
                self.failed = false;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "snapshot leaves invalid retained membership chain",
                ));
            }
            let mut suffix_bytes = 0u64;
            if matches {
                for definition in [LOG, MEMBERSHIPS] {
                    let table = tx.open_table(definition).map_err(io::Error::other)?;
                    for record in table
                        .range((
                            std::ops::Bound::Excluded(boundary.index.0),
                            std::ops::Bound::Unbounded,
                        ))
                        .map_err(io::Error::other)?
                    {
                        suffix_bytes = suffix_bytes
                            .checked_add(record.map_err(io::Error::other)?.1.value().len() as u64)
                            .ok_or_else(|| invalid("install suffix capacity overflow"))?;
                    }
                }
            }
            if !snapshot_install_fits(existing, bytes.len() as u64, membership_size, suffix_bytes) {
                self.failed = false;
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "snapshot and retained suffix exceed owned install capacity",
                ));
            }
            if matches {
                // A matching log identity also fixes the membership prefix. Do
                // not let a validly framed remote object rewrite that history.
                let local_membership = {
                    let memberships = tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
                    let mut prefix = memberships
                        .range(..=boundary.index.0)
                        .map_err(io::Error::other)?;
                    if let Some(record) = prefix.next_back() {
                        record.map_err(io::Error::other)?.1.value().to_vec()
                    } else if snapshot.is_some() {
                        let (old, _) = crate::consensus_storage::decode_owned_snapshot(&get(
                            "compacted_snapshot",
                        )?)
                        .map_err(io::Error::other)?;
                        let old = old
                            .membership
                            .ok_or_else(|| invalid("missing compacted membership"))?;
                        crate::consensus_storage::encode_owned_entry(&Entry {
                            identity: old
                                .introduced_at
                                .ok_or_else(|| invalid("unpositioned compacted membership"))?,
                            payload: EntryPayload::Membership(old.membership),
                        })
                        .map_err(io::Error::other)?
                    } else {
                        Vec::new()
                    }
                };
                if local_membership != membership_bytes {
                    self.failed = false;
                    return Err(invalid(
                        "snapshot conflicts with matching membership prefix",
                    ));
                }
                compact_table(&tx, LOG, boundary.index.0)?;
                compact_table(&tx, MEMBERSHIPS, boundary.index.0)?;
            } else {
                // No common boundary: only an uncommitted suffix may be lost.
                tx.delete_table(LOG).map_err(io::Error::other)?;
                tx.delete_table(MEMBERSHIPS).map_err(io::Error::other)?;
                tx.open_table(LOG).map_err(io::Error::other)?;
                tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
            }
            let effective = {
                let memberships = tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
                memberships.last().map_err(io::Error::other)?.map_or_else(
                    || membership_bytes.clone(),
                    |(_, value)| value.value().to_vec(),
                )
            };
            let commit = if matches && committed.is_some_and(|c| c.index > boundary.index) {
                committed
            } else {
                Some(boundary)
            };
            for (key, value) in [
                ("control", control.as_slice()),
                ("applied_membership", membership_bytes.as_slice()),
                ("membership", effective.as_slice()),
                ("published_snapshot", bytes),
                ("compacted_snapshot", bytes),
            ] {
                meta.insert(key, value).map_err(io::Error::other)?;
            }
            meta.insert("snapshot", encode_position(Some(boundary)).as_slice())
                .map_err(io::Error::other)?;
            meta.insert("applied", encode_position(Some(boundary)).as_slice())
                .map_err(io::Error::other)?;
            meta.insert("commit", encode_position(commit).as_slice())
                .map_err(io::Error::other)?;
            if let Some(receipt) = receipt {
                let mut receipts = meta
                    .get("install_receipts")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing install receipts"))?
                    .value()
                    .to_vec();
                validate_install_receipts(&receipts)?;
                if receipts.len() == MAX_INSTALL_RECEIPTS * INSTALL_RECEIPT_BYTES {
                    receipts.drain(..INSTALL_RECEIPT_BYTES);
                }
                receipts.extend_from_slice(receipt);
                meta.insert("install_receipts", receipts.as_slice())
                    .map_err(io::Error::other)?;
            }
        }
        // All install authority lives in this one immediate transaction. A
        // crash can expose the old state or the complete new state, never a
        // snapshot marker with old control state or a lost acknowledged suffix.
        #[cfg(test)]
        snapshot_install_crash("before-commit");
        tx.commit().map_err(io::Error::other)?;
        #[cfg(test)]
        snapshot_install_crash("after-commit");
        self.failed = false;
        Ok(())
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
        // redb defers freeing deleted pages until later durable transactions.
        // Dropping the read-only capacity preflight cannot advance that horizon.
        // Follow the fixed drain sequence used at the start of redb 4.1's
        // Database::compact, without its unbounded physical page relocation.
        // No read transactions/savepoints escape this exclusive store owner.
        for _ in 0..2 {
            let mut maintenance = self.database.begin_write().map_err(io::Error::other)?;
            maintenance
                .set_durability(Durability::Immediate)
                .map_err(io::Error::other)?;
            maintenance.set_two_phase_commit(true);
            maintenance.commit().map_err(io::Error::other)?;
        }
        // Do not report successful maintenance if either durable drain failed.
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
        self.apply_committed_batch().map(|batch| {
            batch
                .outcomes
                .into_iter()
                .map(|outcome| outcome.response.unwrap_or_default())
                .collect()
        })
    }

    // One dispatcher and one transaction serve both legacy and tagged callers.
    #[allow(clippy::too_many_lines)]
    fn apply_committed_batch(&mut self) -> io::Result<AppliedBatch> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let mut tx = self.database.begin_write().map_err(io::Error::other)?;
        tx.set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        let mut replies = Vec::new();
        let resulting_applied;
        let more;
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
                let mut budget = APPLICATION_ENTRY_BUDGET;
                let mut expected = start;
                let mut bounded = false;
                for record in log
                    .range(start..=committed.index.0)
                    .map_err(io::Error::other)?
                {
                    let (index, bytes) = record.map_err(io::Error::other)?;
                    if replies.len() == 64 {
                        bounded = true;
                        break;
                    }
                    if index.value() != expected {
                        return Err(invalid("missing committed application entry"));
                    }
                    if bytes.value().len() > budget {
                        if replies.is_empty() {
                            return Err(invalid("committed entry exceeds application budget"));
                        }
                        bounded = true;
                        break;
                    }
                    budget -= bytes.value().len();
                    let entry = crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map_err(io::Error::other)?;
                    let reply = match entry.payload {
                        EntryPayload::NoOp => None,
                        EntryPayload::Membership(_) => {
                            membership_bytes = bytes.value().to_vec();
                            None
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
                            Some(crate::consensus_storage::apply_control_command(
                                &mut state,
                                crate::consensus_storage::ApplicationMembership::Owned(&membership),
                                &command,
                            )?)
                        }
                    };
                    applied = Some(entry.identity);
                    expected = expected.saturating_add(1);
                    replies.push(AppliedOutcome {
                        identity: entry.identity,
                        response: reply,
                    });
                }
                if !bounded && applied != Some(committed) {
                    return Err(invalid("missing committed application suffix"));
                }
            }
            resulting_applied = applied;
            more = applied != committed;
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
        Ok(AppliedBatch {
            outcomes: replies,
            applied: resulting_applied,
            more,
        })
    }

    fn outbound_batch(
        &mut self,
        leader: Option<&crate::consensus_model::LeaderProgress>,
        local: NodeId,
        peer: NodeId,
    ) -> Result<
        Result<OutboundAppendBatch, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError>,
        VoteGateError,
    > {
        self.outbound_batch_inner(leader, local, peer, false)
    }

    // Keep authoritative endpoint, predecessor and bounded encoding selection
    // together for the replication and heartbeat views of this same log.
    #[allow(clippy::too_many_lines)]
    fn outbound_batch_inner(
        &mut self,
        leader: Option<&crate::consensus_model::LeaderProgress>,
        local: NodeId,
        peer: NodeId,
        heartbeat: bool,
    ) -> Result<
        Result<OutboundAppendBatch, bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError>,
        VoteGateError,
    > {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        let (_, membership) = self.election_context()?;
        let Some(leader) = leader.filter(|leader| {
            leader.term == self.state.term
                && self.state.voted_for == Some(local)
                && membership.as_ref() == Some(&leader.membership)
        }) else {
            return Ok(Err(RpcError::StaleTerm));
        };
        let Some(next) = leader.next.get(&peer).copied().filter(|_| peer != local) else {
            return Ok(Err(RpcError::Unauthorized));
        };
        self.failed = true;
        let result = (|| -> io::Result<_> {
            let tx = self.database.begin_read().map_err(io::Error::other)?;
            let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let control = meta
                .get("control")
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing control"))?;
            let state = crate::control_state::ControlState::decode_snapshot(control.value())
                .map_err(io::Error::other)?;
            let cluster = state.cluster_id.clone();
            let endpoint = match owned_vote_endpoint(&state, &cluster, peer) {
                Ok(endpoint) => endpoint,
                Err(error) => return Ok(Err(error)),
            };
            let Some(member) = state.members.get(&local.to_string()) else {
                return Ok(Err(RpcError::Unauthorized));
            };
            let snapshot = decode_position(
                meta.get("snapshot")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing snapshot"))?
                    .value(),
            )?;
            if snapshot.is_some_and(|boundary| next <= boundary.index) {
                return Ok(Err(RpcError::Incompatible)); // Requires staged snapshot transfer.
            }
            let previous = match next.0.checked_sub(1) {
                None => None,
                Some(index) if snapshot.is_some_and(|boundary| boundary.index.0 == index) => {
                    snapshot
                }
                Some(index) => {
                    let bytes = log
                        .get(index)
                        .map_err(io::Error::other)?
                        .ok_or_else(|| invalid("missing probe predecessor"))?;
                    Some(
                        crate::consensus_storage::decode_owned_entry(bytes.value())
                            .map_err(io::Error::other)?
                            .identity,
                    )
                }
            };
            let mut entries = Vec::new();
            let mut bytes_left = 256 * 1024usize;
            for item in log
                .range(next.0..)
                .map_err(io::Error::other)?
                .take(if heartbeat { 0 } else { 64 })
            {
                let (_, bytes) = item.map_err(io::Error::other)?;
                if bytes.value().len() > bytes_left {
                    if entries.is_empty() {
                        return Ok(Err(RpcError::Incompatible));
                    }
                    break;
                }
                bytes_left -= bytes.value().len();
                entries.push(
                    crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map_err(io::Error::other)?,
                );
            }
            let commit = decode_position(
                meta.get("commit")
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing commit"))?
                    .value(),
            )?;
            Ok(Ok(OutboundAppendBatch {
                v2: crate::consensus_network::supports_owned_append_v2(member),
                endpoint,
                cluster,
                credential_serial: member.credential_serial.clone(),
                append: AppendRequest {
                    term: leader.term,
                    leader: local,
                    previous,
                    entries,
                    leader_commit: commit,
                },
            }))
        })()
        .map_err(|_| VoteGateError::StorageFailure)?;
        self.failed = false;
        Ok(result)
    }

    fn peer_supports_append_v2(&mut self, peer: NodeId) -> Result<bool, VoteGateError> {
        let state = self
            .read_control_state()
            .map_err(|_| VoteGateError::StorageFailure)?;
        let Some(member) = state.members.get(&peer.to_string()) else {
            return Ok(false);
        };
        Ok(member.cluster_id == state.cluster_id
            && member.state == bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
            && crate::membership::verify_membership_credential(member, crate::now_unix_ms())
                .is_ok()
            && crate::consensus_network::supports_owned_append_v2(member))
    }

    fn admit_replication(
        &mut self,
        leader: Option<&crate::consensus_model::LeaderProgress>,
        local: NodeId,
        request: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    ) -> Result<ReplicationAdmission, VoteGateError> {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::{AppendEntry, RpcError};
        if crate::consensus_network::owned_append_signing_payload(request).is_err() {
            return Ok(Err(RpcError::InvalidRequest));
        }
        let mut decoded = Vec::with_capacity(request.entries.len());
        for wire in &request.entries {
            match wire {
                AppendEntry::Inline { canonical } => {
                    let Ok(entry) = crate::consensus_storage::decode_owned_entry(canonical) else {
                        return Ok(Err(RpcError::InvalidRequest));
                    };
                    decoded.push(entry);
                }
                AppendEntry::Staged { .. } => return Ok(Err(RpcError::Incompatible)),
            }
        }
        let (_, membership) = self.election_context()?;
        let Some(leader) = leader.filter(|leader| {
            leader.term.0 == request.term
                && self.state.term == leader.term
                && self.state.voted_for == Some(local)
                && membership.as_ref() == Some(&leader.membership)
        }) else {
            return Ok(Err(RpcError::StaleTerm));
        };
        let peer = NodeId::from_bytes(
            request
                .recipient
                .as_slice()
                .try_into()
                .map_err(|_| VoteGateError::Stopped)?,
        );
        if peer == local || !leader.matched.contains_key(&peer) {
            return Ok(Err(RpcError::Unauthorized));
        }
        self.failed = true;
        let result = (|| -> io::Result<_> {
            let tx = self.database.begin_read().map_err(io::Error::other)?;
            let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let control = meta.get("control").map_err(io::Error::other)?.ok_or_else(|| invalid("missing control"))?;
            let state = crate::control_state::ControlState::decode_snapshot(control.value()).map_err(io::Error::other)?;
            let endpoint = match owned_vote_endpoint(&state, &request.cluster_id, peer) {
                Ok(endpoint) => endpoint, Err(error) => return Ok(Err(error)),
            };
            let convert = |id: &bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity| -> io::Result<LogIdentity> {
                Ok(LogIdentity { term: Term(id.term), index: crate::consensus_model::LogIndex(id.index),
                    leader: NodeId::from_bytes(id.leader.as_slice().try_into().map_err(|_| invalid("invalid identity"))?) })
            };
            let mut through = request.previous.as_ref().map(convert).transpose()?;
            let snapshot = decode_position(meta.get("snapshot").map_err(io::Error::other)?.ok_or_else(|| invalid("missing snapshot"))?.value())?;
            for position in through.iter().chain(request.leader_commit.as_ref().map(convert).transpose()?.iter()) {
                // The durable snapshot replaces its boundary entry, but never
                // authorizes another identity at that index or a guessed prefix.
                if snapshot == Some(*position) {
                    continue;
                }
                let Some(bytes) = log.get(position.index.0).map_err(io::Error::other)? else { return Ok(Err(RpcError::InvalidRequest)); };
                let entry = crate::consensus_storage::decode_owned_entry(bytes.value()).map_err(io::Error::other)?;
                if entry.identity != *position { return Ok(Err(RpcError::InvalidRequest)); }
            }
            if let Some(claimed) = request.leader_commit.as_ref() {
                let commit = decode_position(meta.get("commit").map_err(io::Error::other)?.ok_or_else(|| invalid("missing commit"))?.value())?;
                if commit.is_none_or(|commit| claimed.index > commit.index.0) { return Ok(Err(RpcError::InvalidRequest)); }
            }
            for entry in decoded {
                if through.is_some_and(|previous| previous.index.0.checked_add(1) != Some(entry.identity.index.0))
                    || (through.is_none() && entry.identity.index.0 != 0) {
                    return Ok(Err(RpcError::InvalidRequest));
                }
                let Some(bytes) = log.get(entry.identity.index.0).map_err(io::Error::other)? else { return Ok(Err(RpcError::InvalidRequest)); };
                if crate::consensus_storage::decode_owned_entry(bytes.value()).map_err(io::Error::other)? != entry {
                    return Ok(Err(RpcError::InvalidRequest));
                }
                through = Some(entry.identity);
            }
            Ok(Ok((endpoint, peer, through, leader.membership.clone())))
        })().map_err(|_| VoteGateError::StorageFailure)?;
        self.failed = false;
        Ok(result)
    }

    fn commit_replicated(
        &mut self,
        leader: &crate::consensus_model::LeaderProgress,
    ) -> Result<Option<LogIdentity>, VoteGateError> {
        let (_, membership) = self.election_context()?;
        if self.state.term != leader.term || membership.as_ref() != Some(&leader.membership) {
            return Ok(None);
        }
        let Some(index) = leader.quorum_index() else {
            return Ok(None);
        };
        self.failed = true;
        let committed = (|| -> io::Result<_> {
            let mut tx = self.database.begin_write().map_err(io::Error::other)?;
            tx.set_durability(Durability::Immediate)
                .map_err(io::Error::other)?;
            let previous = {
                let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
                write_position(&meta, "commit")?
            };
            // A quorum behind the durable commit cannot advance authority and
            // may legitimately refer to entries removed by snapshot compaction.
            if previous.is_some_and(|committed| index <= committed.index) {
                return Ok(None);
            }
            let candidate = {
                let log = tx.open_table(LOG).map_err(io::Error::other)?;
                let bytes = log
                    .get(index.0)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("quorum position missing from log"))?;
                let candidate = crate::consensus_storage::decode_owned_entry(bytes.value())
                    .map_err(io::Error::other)?
                    .identity;
                // Evidence beyond commit must still name authoritative entries.
                // Older matches cannot contribute to this advancing candidate.
                for matched in leader.matched.values().flatten().filter(|matched| {
                    previous.is_none_or(|committed| matched.index > committed.index)
                }) {
                    let bytes = log
                        .get(matched.index.0)
                        .map_err(io::Error::other)?
                        .ok_or_else(|| invalid("replication match missing from log"))?;
                    if crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map_err(io::Error::other)?
                        .identity
                        != *matched
                    {
                        return Err(invalid("replication match identity changed"));
                    }
                }
                candidate
            };
            let advance = {
                let mut meta = tx.open_table(TABLE).map_err(io::Error::other)?;
                let previous = write_position(&meta, "commit")?;
                let advance = candidate.term == self.state.term
                    && previous.is_none_or(|old| candidate.index > old.index);
                if advance {
                    meta.insert("commit", encode_position(Some(candidate)).as_slice())
                        .map_err(io::Error::other)?;
                }
                advance
            };
            tx.commit().map_err(io::Error::other)?;
            Ok(advance.then_some(candidate))
        })()
        .map_err(|_| VoteGateError::StorageFailure)?;
        self.failed = false;
        Ok(committed)
    }

    /// Check predictable local growth before the append transaction. redb exposes
    /// allocation statistics on a write handle, but this preflight makes NO
    /// mutations and drops that handle before returning. Refusal is not an I/O
    /// failure and must leave admission usable for compaction and retry.
    fn has_local_append_capacity(&mut self, entry_bytes: usize) -> Result<bool, VoteGateError> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(VoteGateError::Stopped);
        }
        self.failed = true;
        let capacity = (|| -> io::Result<bool> {
            let tx = self.database.begin_write().map_err(io::Error::other)?;
            let stats = tx.stats().map_err(io::Error::other)?;
            let allocated = stats
                .allocated_pages()
                .checked_mul(stats.page_size() as u64)
                .ok_or_else(|| invalid("proposal allocation overflow"))?;
            let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let control = meta
                .get("control")
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing proposal control state"))?;
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let applied = write_position(&meta, "applied")?;
            let mut pending = entry_bytes as u64;
            let start = applied.map_or(Some(0), |position| position.index.0.checked_add(1));
            if let Some(start) = start {
                for record in log.range(start..).map_err(io::Error::other)? {
                    pending = pending
                        .checked_add(record.map_err(io::Error::other)?.1.value().len() as u64)
                        .ok_or_else(|| invalid("pending proposal byte overflow"))?;
                }
            }
            // Leave half the backend ceiling for geometric allocation growth.
            // Charge 8 copies of the incoming entry: append framing/COW, its
            // possible dedup+workflow response growth and later snapshot copies.
            // Existing canonical control needs six copies of headroom across
            // application, publication and compaction transactions. Add fixed
            // metadata/COW slack; this is a conservative local implementation
            // budget, NOT a negotiated command or workspace-size contract.
            let fits = allocated
                .checked_add(8 * 1024 * 1024)
                .and_then(|bytes| {
                    pending
                        .checked_mul(8)
                        .and_then(|entry| bytes.checked_add(entry))
                })
                .and_then(|bytes| {
                    (control.value().len() as u64)
                        .checked_mul(6)
                        .and_then(|state| bytes.checked_add(state))
                })
                .is_some_and(|bytes| bytes <= MAX_VOTE_PAYLOAD / 2);
            Ok(fits && log.len().map_err(io::Error::other)? < MAX_RETAINED_ENTRIES)
        })()
        .map_err(|_| VoteGateError::StorageFailure)?;
        self.failed = false;
        Ok(capacity)
    }

    fn read_control_state(&mut self) -> io::Result<crate::control_state::ControlState> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let tx = self.database.begin_read().map_err(io::Error::other)?;
        let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
        let bytes = meta
            .get("control")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing canonical control state"))?;
        let state = crate::control_state::ControlState::decode_snapshot(bytes.value())
            .map_err(io::Error::other)?;
        self.failed = false;
        Ok(state)
    }

    fn observe(&mut self) -> Result<OwnedObservation, VoteGateError> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(VoteGateError::Stopped);
        }
        self.failed = true;
        let observation = (|| -> io::Result<_> {
            let tx = self.database.begin_read().map_err(io::Error::other)?;
            let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let get = |key| -> io::Result<Vec<u8>> {
                Ok(meta
                    .get(key)
                    .map_err(io::Error::other)?
                    .ok_or_else(|| invalid("missing observation authority"))?
                    .value()
                    .to_vec())
            };
            let committed = decode_position(&get("commit")?)?;
            let snapshot = decode_position(&get("snapshot")?)?;
            let decode_membership =
                |bytes: &[u8]| -> io::Result<Option<crate::consensus_model::StoredMembership>> {
                    if bytes.is_empty() {
                        return Ok(None);
                    }
                    let entry = crate::consensus_storage::decode_owned_entry(bytes)
                        .map_err(io::Error::other)?;
                    let EntryPayload::Membership(membership) = entry.payload else {
                        return Err(invalid("invalid observation membership"));
                    };
                    Ok(Some(crate::consensus_model::StoredMembership {
                        introduced_at: Some(entry.identity),
                        membership,
                    }))
                };
            let effective_membership = decode_membership(&get("membership")?)?;
            let mut committed_membership = None;
            if let Some(boundary) = committed {
                let memberships = tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
                if let Some(record) = memberships
                    .range(..=boundary.index.0)
                    .map_err(io::Error::other)?
                    .next_back()
                {
                    committed_membership =
                        decode_membership(record.map_err(io::Error::other)?.1.value())?;
                } else if snapshot.is_some() {
                    let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(&get(
                        "compacted_snapshot",
                    )?)
                    .map_err(io::Error::other)?;
                    committed_membership = descriptor.membership;
                }
            }
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let last_log = log
                .last()
                .map_err(io::Error::other)?
                .map(|(_, bytes)| {
                    crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map(|entry| entry.identity)
                })
                .transpose()
                .map_err(io::Error::other)?
                .or(snapshot);
            Ok(OwnedObservation {
                hard_state: self.state,
                local_leader: false,
                election_activity: None,
                effective_membership,
                committed_membership,
                last_log,
                committed,
                applied: decode_position(&get("applied")?)?,
            })
        })()
        .map_err(|_| VoteGateError::StorageFailure)?;
        self.failed = false;
        Ok(observation)
    }

    fn election_context(
        &mut self,
    ) -> Result<(Option<LogIdentity>, Option<Membership>), VoteGateError> {
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
        Ok(context)
    }

    fn campaign(&mut self, local: NodeId) -> Result<Option<RequestVote>, VoteGateError> {
        let (last_log, membership) = self.election_context()?;
        if !membership.is_some_and(|membership| {
            membership
                .configurations()
                .iter()
                .any(|voters| voters.contains(&local))
        }) {
            return Ok(None);
        }
        let term = self
            .state
            .term
            .successor()
            .map_err(|_| VoteGateError::EffectExhausted)?;
        self.persist(HardState {
            term,
            voted_for: Some(local),
        })
        .map_err(|_| VoteGateError::StorageFailure)?;
        Ok(Some(RequestVote {
            term,
            candidate: local,
            last_log,
        }))
    }

    fn vote(&mut self, local: NodeId, request: RequestVote) -> Result<VoteResponse, VoteGateError> {
        let context = self.election_context()?;
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

    // Resolve an already persisted transfer without relying on disposable staging.
    // A match supplies bytes only: append must still reconcile the predecessor.
    fn retained_transfer_entry(
        &mut self,
        reference: &bmux_cluster_plugin_api::cluster_consensus_rpc_types::SealedEntryReference,
        budget: usize,
    ) -> io::Result<Option<Vec<u8>>> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        let identity = crate::consensus_network::transfer_position(&reference.identity)
            .map_err(|error| invalid(&error))?;
        if reference.total_bytes > budget as u64 || reference.checksum.len() != 32 {
            return Err(invalid("invalid retained transfer bounds"));
        }
        self.failed = true;
        let result = (|| -> io::Result<_> {
            let tx = self.database.begin_read().map_err(io::Error::other)?;
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let Some(bytes) = log.get(identity.index.0).map_err(io::Error::other)? else {
                return Ok(None);
            };
            let bytes = bytes.value();
            if bytes.len() as u64 != reference.total_bytes
                || Sha256::digest(bytes).as_slice() != reference.checksum
            {
                return Ok(None);
            }
            let entry =
                crate::consensus_storage::decode_owned_entry(bytes).map_err(io::Error::other)?;
            Ok((entry.identity == identity).then(|| bytes.to_vec()))
        })();
        if result.is_ok() {
            self.failed = false;
        }
        result
    }

    fn activity_membership_valid(
        tx: &redb::ReadTransaction,
        request: &AppendRequest,
        committed: Option<LogIdentity>,
    ) -> io::Result<bool> {
        if !request
            .entries
            .iter()
            .any(|entry| matches!(entry.payload, EntryPayload::Membership(_)))
        {
            return Ok(true);
        }
        let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
        let memberships = tx.open_table(MEMBERSHIPS).map_err(io::Error::other)?;
        let seed_bytes = meta
            .get("applied_membership")
            .map_err(io::Error::other)?
            .ok_or_else(|| invalid("missing activity membership"))?;
        let seed = (!seed_bytes.value().is_empty())
            .then(|| stored_membership(seed_bytes.value()))
            .transpose()?;
        let first = request
            .entries
            .first()
            .ok_or_else(|| invalid("missing proposed suffix"))?
            .identity
            .index
            .0;
        let mut chain = Vec::new();
        for record in memberships.range(..first).map_err(io::Error::other)? {
            let (_, bytes) = record.map_err(io::Error::other)?;
            let membership = stored_membership(bytes.value())?;
            if seed.as_ref().is_none_or(|seed| {
                membership
                    .introduced_at
                    .zip(seed.introduced_at)
                    .is_some_and(|(entry, seed)| entry.index > seed.index)
            }) {
                chain.push(membership);
            }
        }
        for entry in &request.entries {
            if let EntryPayload::Membership(membership) = &entry.payload {
                if seed.as_ref().is_some_and(|seed| {
                    seed.introduced_at
                        .is_some_and(|position| entry.identity.index <= position.index)
                }) {
                    continue;
                }
                chain.push(crate::consensus_model::StoredMembership {
                    introduced_at: Some(entry.identity),
                    membership: membership.clone(),
                });
            }
        }
        Ok(validate_owned_membership_chain(seed.as_ref(), &chain, committed).is_ok())
    }

    // Authentication and eligibility are checked by the executor first. Inspect
    // the pre-append view: PreviousMismatch alone does not inspect the proposed
    // suffix for committed conflicts, payload equivocation or membership errors.
    // These checks gate deadline evidence, not Raft's ordinary repair response.
    fn activity_commit_valid(
        log: &redb::ReadOnlyTable<u64, &[u8]>,
        request: &AppendRequest,
        snapshot: Option<LogIdentity>,
        committed: Option<LogIdentity>,
    ) -> io::Result<bool> {
        let Some(advertised) = request.leader_commit else {
            return Ok(true);
        };
        if advertised.term == request.term && advertised.leader != request.leader {
            return Ok(false);
        }
        let existing = log
            .get(advertised.index.0)
            .map_err(io::Error::other)?
            .map(|bytes| {
                crate::consensus_storage::decode_owned_entry(bytes.value())
                    .map(|entry| entry.identity)
                    .map_err(io::Error::other)
            })
            .transpose()?
            .or_else(|| snapshot.filter(|boundary| boundary.index == advertised.index));
        // A known committed identity is immutable, regardless of term ordering.
        // Snapshot boundaries are committed anchors even without a retained log.
        if committed.is_some_and(|commit| advertised.index <= commit.index)
            && existing.is_some_and(|identity| identity != advertised)
            || snapshot.is_some_and(|boundary| {
                boundary.index == advertised.index && boundary != advertised
            })
        {
            return Ok(false);
        }
        let last = log
            .last()
            .map_err(io::Error::other)?
            .map(|(_, bytes)| {
                crate::consensus_storage::decode_owned_entry(bytes.value())
                    .map(|entry| entry.identity)
                    .map_err(io::Error::other)
            })
            .transpose()?;
        // Different-term suffixes can legitimately be repaired. Same-term
        // leader disagreement at any visible anchor cannot be ordinary repair.
        Ok(existing
            .into_iter()
            .chain(snapshot)
            .chain(last)
            .chain(request.previous)
            .chain(request.entries.iter().map(|entry| entry.identity))
            .all(|identity| {
                identity.term != advertised.term || identity.leader == advertised.leader
            }))
    }

    fn append_activity_valid(&mut self, request: &AppendRequest) -> io::Result<bool> {
        if request.term != self.state.term || request.validate(64, 32 * 1024 * 1024).is_err() {
            return Ok(false);
        }
        self.failed = true;
        let valid = (|| -> io::Result<bool> {
            let tx = self.database.begin_read().map_err(io::Error::other)?;
            let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
            let log = tx.open_table(LOG).map_err(io::Error::other)?;
            let position = |key| -> io::Result<Option<LogIdentity>> {
                decode_position(
                    meta.get(key)
                        .map_err(io::Error::other)?
                        .ok_or_else(|| invalid("missing activity authority"))?
                        .value(),
                )
            };
            let committed = position("commit")?;
            let snapshot = position("snapshot")?;
            if !Self::activity_commit_valid(&log, request, snapshot, committed)? {
                return Ok(false);
            }
            let last = log
                .last()
                .map_err(io::Error::other)?
                .map(|(_, bytes)| {
                    crate::consensus_storage::decode_owned_entry(bytes.value())
                        .map(|entry| entry.identity)
                        .map_err(io::Error::other)
                })
                .transpose()?
                .or(snapshot);
            if last.is_some_and(|last| last.term == request.term && last.leader != request.leader) {
                return Ok(false);
            }
            for proposed in request
                .previous
                .iter()
                .chain(request.entries.iter().map(|entry| &entry.identity))
            {
                let existing = log
                    .get(proposed.index.0)
                    .map_err(io::Error::other)?
                    .map(|bytes| {
                        crate::consensus_storage::decode_owned_entry(bytes.value())
                            .map_err(io::Error::other)
                    })
                    .transpose()?;
                let identity = existing
                    .as_ref()
                    .map(|entry| entry.identity)
                    .or_else(|| snapshot.filter(|boundary| boundary.index == proposed.index));
                if identity.is_some_and(|identity| {
                    identity.term == proposed.term && identity.leader != proposed.leader
                }) || (committed.is_some_and(|commit| proposed.index <= commit.index)
                    && identity != Some(*proposed))
                {
                    return Ok(false);
                }
                if let Some(existing) = existing
                    && let Some(entry) = request
                        .entries
                        .iter()
                        .find(|entry| entry.identity == *proposed)
                    && existing.identity == entry.identity
                    && existing.payload != entry.payload
                {
                    return Ok(false);
                }
            }
            // Matched suffixes receive full candidate-membership validation in
            // append (including its reconciled commit watermark). Only the
            // mismatch fast path needs this extra admission check.
            if let Some(previous) = request.previous {
                let local = log
                    .get(previous.index.0)
                    .map_err(io::Error::other)?
                    .map(|bytes| {
                        crate::consensus_storage::decode_owned_entry(bytes.value())
                            .map(|entry| entry.identity)
                            .map_err(io::Error::other)
                    })
                    .transpose()?
                    .or_else(|| snapshot.filter(|boundary| boundary.index == previous.index));
                if local != Some(previous) {
                    return Self::activity_membership_valid(&tx, request, committed);
                }
            }
            Ok(true)
        })()?;
        self.failed = false;
        Ok(valid)
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
            let committed = write_position(&meta, "commit")?;
            let mut log = transaction.open_table(LOG).map_err(io::Error::other)?;
            let mut memberships = transaction
                .open_table(MEMBERSHIPS)
                .map_err(io::Error::other)?;
            let entries = append_lookup_entries(&log, request, committed)?;
            let snapshot = write_position(&meta, "snapshot")?;
            decision = self.state.evaluate_append(
                request,
                (committed, snapshot),
                |index| Ok(entries.get(&index.0)),
                limits,
            );
            if let Ok(AppendOutcome::Reconciled(AppendReconciliation::Matched {
                replace_from,
                ..
            })) = decision.outcome
            {
                // Only the reconciled commit is verified coverage.
                if let Err(error) = validate_candidate_memberships(
                    &meta,
                    &memberships,
                    request,
                    replace_from,
                    decision.commit,
                ) {
                    if error.kind() == io::ErrorKind::InvalidInput {
                        self.failed = false;
                    }
                    return Err(error);
                }
            }
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

    /// Durable authority in the shape the pure engine recovers from.
    /// # Errors
    /// Read or decode failure is an explicit recovery error, never empty state.
    pub fn recovered(&mut self) -> io::Result<crate::consensus_engine::Recovered> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        let tx = self.database.begin_read().map_err(io::Error::other)?;
        let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
        let get = |key| -> io::Result<Vec<u8>> {
            Ok(meta
                .get(key)
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing recovery authority"))?
                .value()
                .to_vec())
        };
        let snapshot = match decode_position(&get("snapshot")?)? {
            None => None,
            Some(boundary) => {
                let (descriptor, _) =
                    crate::consensus_storage::decode_owned_snapshot(&get("compacted_snapshot")?)
                        .map_err(io::Error::other)?;
                let membership = descriptor
                    .membership
                    .ok_or_else(|| invalid("compacted snapshot lacks membership"))?;
                Some((boundary, membership))
            }
        };
        let log = tx.open_table(LOG).map_err(io::Error::other)?;
        let mut entries = Vec::new();
        for record in log.iter().map_err(io::Error::other)? {
            let (_, bytes) = record.map_err(io::Error::other)?;
            entries.push(
                crate::consensus_storage::decode_owned_entry(bytes.value())
                    .map_err(io::Error::other)?,
            );
        }
        Ok(crate::consensus_engine::Recovered {
            hard_state: self.state,
            snapshot,
            entries,
            commit: decode_position(&get("commit")?)?,
            applied: decode_position(&get("applied")?)?,
        })
    }

    /// Execute one engine write (hard state, suffix replacement, commit
    /// watermark) as a single immediate transaction, in the order issued.
    /// # Errors
    /// Any failure permanently stops the handle; the engine must stop too.
    pub fn write(&mut self, write: &crate::consensus_engine::Write) -> io::Result<()> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        if let Some(state) = write.hard_state
            && (state.term < self.state.term
                || (state.term == self.state.term
                    && self.state.voted_for.is_some()
                    && state.voted_for != self.state.voted_for))
        {
            return Err(invalid("vote authority regression"));
        }
        self.failed = true;
        let mut transaction = self.database.begin_write().map_err(io::Error::other)?;
        transaction
            .set_durability(Durability::Immediate)
            .map_err(io::Error::other)?;
        {
            let mut meta = transaction.open_table(TABLE).map_err(io::Error::other)?;
            if let Some(state) = write.hard_state {
                meta.insert("vote", encode(state)?.as_slice())
                    .map_err(io::Error::other)?;
            }
            if write.truncate_from.is_some() || !write.entries.is_empty() {
                let mut log = transaction.open_table(LOG).map_err(io::Error::other)?;
                let mut memberships = transaction
                    .open_table(MEMBERSHIPS)
                    .map_err(io::Error::other)?;
                if let Some(first) = write.truncate_from {
                    log.retain_in(first.0.., |_, _| false)
                        .map_err(io::Error::other)?;
                    memberships
                        .retain_in(first.0.., |_, _| false)
                        .map_err(io::Error::other)?;
                }
                for entry in &write.entries {
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
                if log.len().map_err(io::Error::other)? > MAX_RETAINED_ENTRIES {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "owned log requires compaction",
                    ));
                }
                let fallback = application_membership_bytes(&meta)?;
                let latest = memberships
                    .last()
                    .map_err(io::Error::other)?
                    .map_or(fallback, |(_, bytes)| bytes.value().to_vec());
                meta.insert("membership", latest.as_slice())
                    .map_err(io::Error::other)?;
            }
            if let Some(commit) = write.commit {
                meta.insert("commit", encode_position(Some(commit)).as_slice())
                    .map_err(io::Error::other)?;
            }
        }
        transaction.commit().map_err(io::Error::other)?;
        if let Some(state) = write.hard_state {
            self.state = state;
        }
        self.failed = false;
        Ok(())
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
fn snapshot_install_crash(point: &str) {
    if std::env::var("BMUX_OWNED_INSTALL_CRASH").as_deref() == Ok(point) {
        std::process::exit(75);
    }
}

#[cfg(test)]
fn application_crash(point: &str) {
    if std::env::var("BMUX_OWNED_APPLY_CRASH").as_deref() == Ok(point) {
        std::process::exit(74);
    }
}

#[cfg(test)]
mod tests {
    fn election_executor(path: &Path) -> (VoteExecutor, NodeId) {
        let local = NodeId::from(1);
        let membership = Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap();
        let store = OwnedVoteStore::bootstrap(path, "cluster", local, membership).unwrap();
        (VoteExecutor::new(DurableVoteGate::new(1, store)), local)
    }

    #[tokio::test]
    async fn election_deadline_checks_activity_membership_and_term_under_admission() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (executor, local) = election_executor(&path);
        let observed = executor.observe(local).await.unwrap();
        let membership = observed.effective_membership.unwrap();
        assert!(observed.election_activity.is_none());
        // Model a completed authenticated admission, not a watch notification.
        executor
            .run(move |gate| {
                gate.record_election_activity(local, ElectionActivityKind::GrantedVote)
            })
            .await
            .unwrap();
        assert!(
            executor
                .campaign_if_unchanged(local, observed.hard_state, membership.clone(), None)
                .await
                .unwrap()
                .is_none()
        );
        let activity = executor
            .observe(local)
            .await
            .unwrap()
            .election_activity
            .unwrap();
        let mut wrong_membership = membership.clone();
        wrong_membership.introduced_at = None;
        assert!(
            executor
                .campaign_if_unchanged(
                    local,
                    observed.hard_state,
                    wrong_membership,
                    Some(activity.sequence),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            executor
                .campaign_if_unchanged(
                    NodeId::from(2),
                    observed.hard_state,
                    membership.clone(),
                    Some(activity.sequence),
                )
                .await
                .unwrap()
                .is_none()
        );
        let request = executor
            .campaign_if_unchanged(
                local,
                observed.hard_state,
                membership.clone(),
                Some(activity.sequence),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(request.term, Term(1));
        assert!(
            executor
                .campaign_if_unchanged(
                    local,
                    observed.hard_state,
                    membership,
                    Some(activity.sequence),
                )
                .await
                .unwrap()
                .is_none()
        );
        executor.shutdown().await.unwrap();
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(
            store.state(),
            HardState {
                term: Term(1),
                voted_for: Some(local)
            }
        );
        let restarted = VoteExecutor::new(DurableVoteGate::new(2, store));
        assert!(
            restarted
                .observe(local)
                .await
                .unwrap()
                .election_activity
                .is_none()
        );
        restarted.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn initialized_leader_rejects_deadline_from_same_term_campaign() {
        let root = tempfile::tempdir().unwrap();
        let (executor, local) = election_executor(&root.path().join("authority"));
        executor.campaign(local).await.unwrap().unwrap();
        let timeout = executor.observe(local).await.unwrap();
        assert!(!timeout.local_leader);
        executor.initialize_leader().await.unwrap().unwrap();
        let leader = executor.observe(local).await.unwrap();
        assert!(leader.local_leader);
        assert_eq!(leader.hard_state, timeout.hard_state);
        assert_eq!(leader.effective_membership, timeout.effective_membership);
        assert_eq!(leader.election_activity, timeout.election_activity);
        assert!(
            executor
                .campaign_if_unchanged(
                    local,
                    timeout.hard_state,
                    timeout.effective_membership.unwrap(),
                    timeout.election_activity.map(|activity| activity.sequence),
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(executor.observe(local).await.unwrap(), leader);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn competing_deadlines_cannot_both_increment_term() {
        let root = tempfile::tempdir().unwrap();
        let (executor, local) = election_executor(&root.path().join("authority"));
        let observed = executor.observe(local).await.unwrap();
        let membership = observed.effective_membership.unwrap();
        let results: [_; 2] = tokio::join!(
            executor.campaign_if_unchanged(local, observed.hard_state, membership.clone(), None),
            executor.campaign_if_unchanged(local, observed.hard_state, membership, None),
        )
        .into();
        let mut campaigns = 0;
        for result in results {
            match result {
                Ok(Some(_)) => campaigns += 1,
                Ok(None) | Err(VoteGateError::Busy) => {}
                other => panic!("unexpected deadline result: {other:?}"),
            }
        }
        assert_eq!(campaigns, 1);
        assert_eq!(
            executor.observe(local).await.unwrap().hard_state.term,
            Term(1)
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_activity_caller_cannot_lose_admitted_evidence() {
        let root = tempfile::tempdir().unwrap();
        let (executor, local) = election_executor(&root.path().join("authority"));
        let executor = std::sync::Arc::new(executor);
        let owner = executor.clone();
        let (started, receive) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            owner
                .run(move |gate| {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                    gate.record_election_activity(local, ElectionActivityKind::GrantedVote)
                })
                .await
        });
        receive.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match executor.observe(local).await {
                    Ok(observed) => {
                        assert_eq!(observed.election_activity.unwrap().sequence, 1);
                        break;
                    }
                    Err(VoteGateError::Busy) => tokio::task::yield_now().await,
                    other => panic!("unexpected observation: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        executor.shutdown().await.unwrap();
    }

    #[test]
    fn election_activity_sequence_is_checked_and_conflicting_leaders_do_not_reset() {
        let root = tempfile::tempdir().unwrap();
        let store = OwnedVoteStore::create(&root.path().join("authority"), "cluster").unwrap();
        let mut gate = DurableVoteGate::new(1, store);
        let peer = NodeId::from(1);
        gate.record_election_activity(peer, ElectionActivityKind::AuthenticatedLeader)
            .unwrap();
        let first = gate.election_activity.unwrap();
        gate.record_election_activity(NodeId::from(2), ElectionActivityKind::AuthenticatedLeader)
            .unwrap();
        assert_eq!(gate.election_activity, Some(first));
        gate.record_election_activity(peer, ElectionActivityKind::AuthenticatedLeader)
            .unwrap();
        assert_eq!(gate.election_activity.unwrap().sequence, 2);
        gate.election_activity.as_mut().unwrap().sequence = u64::MAX;
        assert!(matches!(
            gate.record_election_activity(peer, ElectionActivityKind::GrantedVote),
            Err(VoteGateError::EffectExhausted)
        ));
        assert!(gate.store.failed);
    }

    #[tokio::test]
    async fn staging_shutdown_joins_expiry_owner() {
        let parent = std::env::temp_dir().canonicalize().unwrap();
        let root = tempfile::tempdir_in(parent).unwrap();
        let directory = root.path().join("staging");
        std::fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (executor, _) = election_executor(&root.path().join("authority"));
        executor.configure_staging(directory, 16, 32).await.unwrap();
        let expiry = executor
            .run(|gate| Ok(gate.staging_expiry.as_ref().unwrap().abort_handle()))
            .await
            .unwrap();
        executor.shutdown().await.unwrap();
        assert!(expiry.is_finished());
    }

    #[tokio::test]
    async fn outbound_vote_preparation_waits_fairly_without_queueing_inbound() {
        let root = tempfile::tempdir().unwrap();
        let store = OwnedVoteStore::create(&root.path().join("authority"), "cluster").unwrap();
        let executor = std::sync::Arc::new(VoteExecutor::new(DurableVoteGate::new(1, store)));
        let guard = executor.gate.clone().lock_owned().await;
        let permit = executor.outbound_votes.clone().try_acquire_owned().unwrap();
        let owner = executor.clone();
        let (ready, started) = tokio::sync::oneshot::channel();
        let preparation = tokio::spawn(async move {
            let _ = ready.send(());
            owner
                .prepare_outbound(&permit, |gate| Ok(gate.store.state()))
                .await
        });
        started.await.unwrap();
        // Give the preparation a chance to enter its bounded FIFO wait while
        // the gate is held; immediate admission would already return Busy.
        tokio::task::yield_now().await;
        assert!(!preparation.is_finished());
        assert!(matches!(
            executor.observe(NodeId::from(1)).await,
            Err(VoteGateError::Busy)
        ));
        drop(guard);
        let state = preparation.await.unwrap().unwrap();
        assert_eq!(
            state,
            HardState {
                term: Term(0),
                voted_for: None
            }
        );
        assert_eq!(executor.outbound_votes.available_permits(), 16);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn outbound_preparation_cancellation_releases_bounded_slot_and_waiter() {
        let root = tempfile::tempdir().unwrap();
        let store = OwnedVoteStore::create(&root.path().join("authority"), "cluster").unwrap();
        let executor = std::sync::Arc::new(VoteExecutor::new(DurableVoteGate::new(1, store)));
        let guard = executor.gate.clone().lock_owned().await;
        let permit = executor.outbound_votes.clone().try_acquire_owned().unwrap();
        let owner = executor.clone();
        let preparation = tokio::spawn(async move {
            owner
                .prepare_outbound(&permit, |_| -> Result<(), VoteGateError> {
                    panic!("cancelled preparation ran")
                })
                .await
        });
        tokio::task::yield_now().await;
        preparation.abort();
        assert!(preparation.await.unwrap_err().is_cancelled());
        assert_eq!(executor.outbound_votes.available_permits(), 16);
        drop(guard);
        executor.observe(NodeId::from(1)).await.unwrap();
        executor.shutdown().await.unwrap();
    }

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
    fn remote_snapshot_receipts_reject_future_applied_conflict_and_term_on_read_and_reopen() {
        for corruption in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("authority");
            let (mut store, _, boundary) = snapshot_install_fixture(&path);
            let mut receipt = [0; INSTALL_RECEIPT_BYTES];
            let invalid_boundary = match corruption {
                0 => LogIdentity {
                    index: crate::consensus_model::LogIndex(boundary.index.0 + 1),
                    ..boundary
                },
                1 => LogIdentity {
                    leader: NodeId::from(2),
                    ..boundary
                },
                _ => boundary,
            };
            receipt[104..152].copy_from_slice(&encode_position(Some(invalid_boundary)));
            let term = store.state().term.0 + u64::from(corruption == 2);
            receipt[152..160].copy_from_slice(&term.to_le_bytes());
            validate_install_receipts(&receipt).unwrap();
            let tx = store.database.begin_write().unwrap();
            tx.open_table(TABLE)
                .unwrap()
                .insert("install_receipts", receipt.as_slice())
                .unwrap();
            tx.commit().unwrap();
            assert!(store.install_replay(&receipt).is_err());
            assert!(store.failed);
            drop(store);
            assert!(OwnedVoteStore::open(&path, "cluster").is_err());
        }
    }

    #[test]
    fn remote_snapshot_receipt_window_corruption_and_old_format_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (mut store, bytes, boundary) = snapshot_install_fixture(&path);
        let mut first = [0; INSTALL_RECEIPT_BYTES];
        let mut latest = first;
        for sequence in 0..=MAX_INSTALL_RECEIPTS as u64 {
            let mut receipt = [0; INSTALL_RECEIPT_BYTES];
            receipt[..16].copy_from_slice(uuid::Uuid::from_u128(u128::from(sequence)).as_bytes());
            receipt[16..48].copy_from_slice(boundary.leader.as_bytes());
            receipt[64..72].copy_from_slice(&sequence.to_le_bytes());
            receipt[72..104].copy_from_slice(&Sha256::digest(&bytes));
            receipt[104..152].copy_from_slice(&encode_position(Some(boundary)));
            receipt[152..160].copy_from_slice(&store.state().term.0.to_le_bytes());
            if sequence == 0 {
                first = receipt;
            }
            store
                .install_snapshot_recorded(store.state(), &bytes, Some(&receipt))
                .unwrap();
            latest = receipt;
        }
        assert_eq!(store.install_replay(&first).unwrap(), None);
        assert_eq!(store.install_replay(&latest).unwrap(), Some(true));
        let mut conflict = latest;
        conflict[72] ^= 1;
        assert_eq!(store.install_replay(&conflict).unwrap(), Some(false));
        drop(store);
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.install_replay(&latest).unwrap(), Some(true));
        {
            let tx = store.database.begin_write().unwrap();
            tx.open_table(TABLE)
                .unwrap()
                .insert("format", &b"BMOWNV05"[..])
                .unwrap();
            tx.commit().unwrap();
        }
        drop(store);
        assert!(OwnedVoteStore::open(&path, "cluster").is_err());
        assert!(validate_install_receipts(&[0; 1]).is_err());
        assert!(
            validate_install_receipts(&[0; INSTALL_RECEIPT_BYTES * (MAX_INSTALL_RECEIPTS + 1)])
                .is_err()
        );
    }

    // Exercise candidate reconstruction, confirmed coverage and snapshot seeding
    // through the actual append/install/recovery owners, not only pure validators.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn membership_chain_rejects_illegal_changes_and_preserves_legal_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let other = NodeId::from(2);
        let old = Membership::new(
            vec![vec![node]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let joint = Membership::new(
            vec![vec![node], vec![node, other]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let final_config = Membership::new(
            vec![vec![node, other]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let mut store = OwnedVoteStore::bootstrap(&path, "cluster", node, old.clone()).unwrap();
        store.apply_committed().unwrap();
        let identity = |term, index| LogIdentity {
            term: Term(term),
            leader: node,
            index: crate::consensus_model::LogIndex(index),
        };
        let genesis = identity(0, 0);
        let request = |entries, commit| AppendRequest {
            term: Term(1),
            leader: node,
            previous: Some(genesis),
            entries,
            leader_commit: commit,
        };
        let member = |identity, membership| Entry {
            identity,
            payload: EntryPayload::Membership(membership),
        };
        let before = store.observe().unwrap();
        assert!(
            store
                .append(
                    &request(
                        vec![member(identity(1, 1), final_config.clone())],
                        Some(identity(1, 1))
                    ),
                    (8, 8192)
                )
                .is_err()
        );
        assert!(!store.failed);
        assert_eq!(store.observe().unwrap(), before);
        // Future index 99 is NOT verified coverage of joint entry 1: only
        // the predecessor of an empty heartbeat is matched.
        store
            .append(
                &request(vec![member(identity(1, 1), joint.clone())], Some(genesis)),
                (8, 8192),
            )
            .unwrap();
        let before = store.observe().unwrap();
        let nested = AppendRequest {
            term: Term(1),
            leader: node,
            previous: Some(identity(1, 1)),
            entries: vec![member(identity(1, 2), joint.clone())],
            leader_commit: Some(identity(1, 2)),
        };
        assert!(store.append(&nested, (8, 8192)).is_err());
        let wrong_final = AppendRequest {
            entries: vec![member(identity(1, 2), old.clone())],
            ..nested.clone()
        };
        assert!(store.append(&wrong_final, (8, 8192)).is_err());
        let unconfirmed = AppendRequest {
            previous: Some(genesis),
            entries: vec![
                member(identity(1, 1), joint),
                member(identity(1, 2), final_config.clone()),
            ],
            leader_commit: Some(genesis),
            ..nested.clone()
        };
        assert!(store.append(&unconfirmed, (8, 8192)).is_err());
        assert_eq!(store.observe().unwrap(), before);
        // A lagging follower may accept joint + final together when the exact
        // joint identity is covered by the reconciled leader commit.
        let valid = AppendRequest {
            leader_commit: Some(identity(1, 1)),
            ..unconfirmed
        };
        assert!(store.append(&valid, (8, 8192)).unwrap().outcome.is_ok());
        assert!(store.append(&valid, (8, 8192)).unwrap().outcome.is_ok());
        // Replace only the uncommitted final suffix, preserving the joint.
        let replacement = AppendRequest {
            term: Term(2),
            leader: node,
            previous: Some(identity(1, 1)),
            entries: vec![member(identity(2, 2), final_config)],
            leader_commit: Some(identity(1, 1)),
        };
        store.append(&replacement, (8, 8192)).unwrap();
        store.apply_committed().unwrap();
        let snapshot = store.publish_snapshot().unwrap();
        store.compact_snapshot().unwrap();
        // The compacted joint seed legally precedes retained uniform final.
        store.install_snapshot(store.state(), &snapshot).unwrap();
        drop(store);
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(
            store
                .observe()
                .unwrap()
                .effective_membership
                .unwrap()
                .introduced_at,
            Some(identity(2, 2))
        );
        // Corrupt the indexed and canonical entry consistently: recovery must
        // reject the illegal chain even though both tables agree byte-for-byte.
        let corrupt =
            crate::consensus_storage::encode_owned_entry(&member(identity(2, 2), old)).unwrap();
        let tx = store.database.begin_write().unwrap();
        tx.open_table(LOG)
            .unwrap()
            .insert(2, corrupt.as_slice())
            .unwrap();
        tx.open_table(MEMBERSHIPS)
            .unwrap()
            .insert(2, corrupt.as_slice())
            .unwrap();
        tx.open_table(TABLE)
            .unwrap()
            .insert("membership", corrupt.as_slice())
            .unwrap();
        tx.commit().unwrap();
        drop(store);
        assert!(OwnedVoteStore::open(&path, "cluster").is_err());
    }

    #[test]
    fn membership_chain_verified_commit_not_advertised_future_index() {
        let root = tempfile::tempdir().unwrap();
        let node = NodeId::from(1);
        let other = NodeId::from(2);
        let old = Membership::new(
            vec![vec![node]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let joint = Membership::new(
            vec![vec![node], vec![node, other]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let mut store =
            OwnedVoteStore::bootstrap(&root.path().join("authority"), "cluster", node, old)
                .unwrap();
        let identity = |index| LogIdentity {
            term: Term(1),
            leader: node,
            index: crate::consensus_model::LogIndex(index),
        };
        store
            .append(
                &AppendRequest {
                    term: Term(1),
                    leader: node,
                    previous: Some(LogIdentity {
                        term: Term(0),
                        ..identity(0)
                    }),
                    entries: vec![Entry {
                        identity: identity(1),
                        payload: EntryPayload::Membership(joint),
                    }],
                    leader_commit: None,
                },
                (8, 8192),
            )
            .unwrap();
        let before = store.observe().unwrap();
        // This heartbeat matches genesis only. A future advertised commit must
        // not retroactively validate a nested/unfinished transition.
        store
            .append(
                &AppendRequest {
                    term: Term(1),
                    leader: node,
                    previous: Some(LogIdentity {
                        term: Term(0),
                        ..identity(0)
                    }),
                    entries: Vec::new(),
                    leader_commit: Some(identity(99)),
                },
                (8, 8192),
            )
            .unwrap();
        assert_eq!(store.observe().unwrap().committed, before.committed);
        let final_config = Membership::new(
            vec![vec![node, other]],
            vec![(node, String::new()), (other, String::new())],
        )
        .unwrap();
        let request = AppendRequest {
            term: Term(1),
            leader: node,
            previous: Some(identity(1)),
            entries: vec![Entry {
                identity: identity(2),
                payload: EntryPayload::Membership(final_config),
            }],
            leader_commit: None,
        };
        assert!(store.append(&request, (8, 8192)).is_err());
        assert!(!store.failed);
    }

    #[derive(Default)]
    struct InstallCaller(
        std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        crate::durable_storage::TestScope,
    );
    impl bmux_plugin::ServiceCaller for InstallCaller {
        fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
            Some(&self.1.connection)
        }

        fn call_service_raw(
            &self,
            _: &str,
            _: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            use bmux_plugin_sdk::{decode_service_message, encode_service_message};
            match (interface, operation) {
                ("storage-query/v1", "get") => {
                    let request: bmux_plugin_sdk::StorageGetRequest =
                        decode_service_message(&payload)?;
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse {
                        value: self.0.lock().unwrap().get(request.key.as_str()).cloned(),
                    })
                }
                ("storage-command/v1", "set") => {
                    let request: bmux_plugin_sdk::StorageSetRequest =
                        decode_service_message(&payload)?;
                    self.0
                        .lock()
                        .unwrap()
                        .insert(request.key.as_str().into(), request.value);
                    encode_service_message(&())
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "install test",
                }),
            }
        }
        fn execute_kernel_request(
            &self,
            _: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "install test kernel",
            })
        }
    }

    type HeldAuthority = tokio::sync::OwnedMutexGuard<Option<DurableVoteGate>>;

    struct AppendV2Endpoint {
        source: std::sync::Arc<InstallCaller>,
        target: std::sync::Arc<InstallCaller>,
        receiver: std::sync::Arc<VoteExecutor>,
        target_id: NodeId,
        calls: std::sync::atomic::AtomicUsize,
        hold_sender: std::sync::Mutex<Option<std::sync::Arc<VoteExecutor>>>,
        held_guard: std::sync::Mutex<Option<HeldAuthority>>,
    }

    impl bmux_plugin::ServiceCaller for AppendV2Endpoint {
        fn host_connection(&self) -> Option<&bmux_plugin_sdk::HostConnectionInfo> {
            bmux_plugin::ServiceCaller::host_connection(self.source.as_ref())
        }

        fn execute_kernel_request(
            &self,
            request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            bmux_plugin::ServiceCaller::execute_kernel_request(self.source.as_ref(), request)
        }

        fn call_service_raw(
            &self,
            capability: &str,
            kind: bmux_plugin_sdk::ServiceKind,
            interface: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            use bmux_connections_plugin_api::{
                connection_types::ConnectionError, connections_commands,
            };
            use bmux_plugin_sdk::{decode_service_message, encode_service_message};
            if interface != connections_commands::INTERFACE_ID.as_str() {
                return bmux_plugin::ServiceCaller::call_service_raw(
                    self.source.as_ref(),
                    capability,
                    kind,
                    interface,
                    operation,
                    payload,
                );
            }
            assert_eq!(operation, connections_commands::OP_INVOKE_SERVICE.as_str());
            let request: connections_commands::client::InvokeServiceRequest =
                decode_service_message(bmux_plugin_sdk::service_contract::decode_for(
                    interface,
                    connections_commands::SERVICE_CONTRACT,
                    &payload,
                )?)?;
            let invocation = request.invocation;
            let provider = crate::consensus_network::OwnedAppendService {
                caller: self.target.clone(),
                local: self.target_id,
                executor: self.receiver.clone(),
            };
            let result = if invocation.interface_id
                == bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID.as_str()
            {
                use bmux_cluster_plugin_api::cluster_consensus_rpc_v2::ClusterConsensusRpcService;
                assert_eq!(invocation.operation, "append");
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let request: bmux_cluster_plugin_api::cluster_consensus_rpc_v2::client::AppendRequest = decode_service_message(&invocation.payload)?;
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(provider.append(request.request))
                });
                let held_sender = self.hold_sender.lock().unwrap().take();
                if let Some(sender) = held_sender {
                    let guard = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(sender.gate.clone().lock_owned())
                    });
                    *self.held_guard.lock().unwrap() = Some(guard);
                }
                encode_service_message(&result)?
            } else if invocation.interface_id
                == bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID.as_str()
            {
                use bmux_cluster_plugin_api::cluster_consensus_rpc::ClusterConsensusRpcService;
                // V1 vote remains its existing contract; append fallback is forbidden.
                assert_eq!(invocation.operation, "vote");
                let request: bmux_cluster_plugin_api::cluster_consensus_rpc::client::VoteRequest =
                    decode_service_message(&invocation.payload)?;
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(provider.vote(request.request))
                });
                encode_service_message(&result)?
            } else {
                assert_eq!(invocation.interface_id, "cluster-peer-auth/v1");
                assert_eq!(invocation.operation, "challenge");
                let request: bmux_cluster_plugin_api::cluster_peer_auth::client::ChallengeRequest =
                    decode_service_message(&invocation.payload)?;
                let state = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(self.receiver.read_control_state())
                })
                .unwrap();
                let challenge = crate::membership::create_peer_auth_challenge_for_members(
                    self.target.as_ref(),
                    &request,
                    &state.members.into_values().collect::<Vec<_>>(),
                )
                .unwrap();
                encode_service_message(&challenge)?
            };
            let response = encode_service_message(&Result::<Vec<u8>, ConnectionError>::Ok(result))?;
            Ok(bmux_plugin_sdk::service_contract::encode_response(
                &response,
            ))
        }
    }

    fn append_v2_store(
        path: &Path,
        members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
        local: NodeId,
        peer: NodeId,
        membership: &Membership,
    ) -> std::sync::Arc<VoteExecutor> {
        let mut store =
            OwnedVoteStore::bootstrap(path, &members[0].cluster_id, local, membership.clone())
                .unwrap();
        store.apply_committed().unwrap();
        // Commit legal joint/uniform transitions, not fabricated voter maps.
        let nodes = || {
            membership
                .nodes()
                .iter()
                .map(|(node, endpoint)| (*node, endpoint.clone()))
                .collect()
        };
        let joint = Membership::new(vec![vec![local], vec![local, peer]], nodes()).unwrap();
        let uniform = Membership::new(vec![vec![local, peer]], nodes()).unwrap();
        for (index, configuration) in [(1, joint), (2, uniform)] {
            let previous = store.observe().unwrap().last_log;
            let identity = LogIdentity {
                term: Term(0),
                leader: local,
                index: crate::consensus_model::LogIndex(index),
            };
            store
                .append(
                    &AppendRequest {
                        term: Term(0),
                        leader: local,
                        previous,
                        entries: vec![Entry {
                            identity,
                            payload: EntryPayload::Membership(configuration),
                        }],
                        leader_commit: Some(identity),
                    },
                    (1, 4096),
                )
                .unwrap();
            store.apply_committed().unwrap();
        }
        let mut state = store.read_control_state().unwrap();
        state.members = members
            .iter()
            .cloned()
            .map(|member| (member.node_id.clone(), member))
            .collect();
        let tx = store.database.begin_write().unwrap();
        tx.open_table(TABLE)
            .unwrap()
            .insert("control", state.encode_snapshot().unwrap().as_slice())
            .unwrap();
        tx.commit().unwrap();
        std::sync::Arc::new(VoteExecutor::new(DurableVoteGate::new(1, store)))
    }

    fn provision_append_members(
        caller: &InstallCaller,
        target: &InstallCaller,
        members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
    ) {
        let mut provisioned: serde_json::Value = serde_json::from_slice(
            caller
                .0
                .lock()
                .unwrap()
                .get(crate::membership::MEMBERSHIP_STATE_STORAGE_KEY)
                .unwrap(),
        )
        .unwrap();
        provisioned["members"] = serde_json::to_value(
            members
                .iter()
                .cloned()
                .map(|member| (member.node_id.clone(), member))
                .collect::<std::collections::BTreeMap<_, _>>(),
        )
        .unwrap();
        let provisioned = serde_json::to_vec(&provisioned).unwrap();
        caller.0.lock().unwrap().insert(
            crate::membership::MEMBERSHIP_STATE_STORAGE_KEY.into(),
            provisioned.clone(),
        );
        target.0.lock().unwrap().insert(
            crate::membership::MEMBERSHIP_STATE_STORAGE_KEY.into(),
            provisioned,
        );
    }

    fn append_v2_pair(root: &Path) -> (std::sync::Arc<VoteExecutor>, AppendV2Endpoint, NodeId) {
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            mut members,
            ..
        } = authenticated_election_fixture();
        // The fixture executor is replaced by two explicitly provisioned stores;
        // use its genuine issued credential and identity, not synthetic auth maps.
        drop(executor);
        let target = std::sync::Arc::new(InstallCaller::default());
        let target_identity =
            crate::membership::load_or_create_node_identity(target.as_ref()).unwrap();
        target.0.lock().unwrap().insert(
            crate::membership::CLUSTER_ID_STORAGE_KEY.into(),
            caller
                .0
                .lock()
                .unwrap()
                .get(crate::membership::CLUSTER_ID_STORAGE_KEY)
                .unwrap()
                .clone(),
        );
        let local = *identity.node_id();
        let peer = *target_identity.node_id();
        let member = &mut members[0];
        member
            .negotiated_protocol
            .features
            .push(bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID.to_string());
        *member = crate::membership::issue_membership_credential(
            &identity,
            member.cluster_id.parse().unwrap(),
            member.node_id.clone(),
            member.public_key.clone(),
            member.capabilities.clone(),
            member.negotiated_protocol.clone(),
            crate::now_unix_ms(),
        )
        .unwrap();
        member.endpoint = Some("tls://source:7443".into());
        let mut learner = crate::membership::issue_membership_credential(
            &identity,
            member.cluster_id.parse().unwrap(),
            peer.to_string(),
            target_identity.public_key().to_string(),
            member.capabilities.clone(),
            member.negotiated_protocol.clone(),
            crate::now_unix_ms(),
        )
        .unwrap();
        learner.endpoint = Some("tls://target:7443".into());
        members.push(learner);
        provision_append_members(caller.as_ref(), target.as_ref(), &members);
        let membership = Membership::new(
            vec![vec![local]],
            vec![
                (local, "tls://source:7443".into()),
                (peer, "tls://target:7443".into()),
            ],
        )
        .unwrap();
        let open = |name| append_v2_store(&root.join(name), &members, local, peer, &membership);
        let sender = open("sender");
        let receiver = open("receiver");
        (
            sender,
            AppendV2Endpoint {
                source: caller,
                target,
                receiver,
                target_id: peer,
                calls: std::sync::atomic::AtomicUsize::new(0),
                hold_sender: std::sync::Mutex::new(None),
                held_guard: std::sync::Mutex::new(None),
            },
            local,
        )
    }

    async fn elect_append_v2_sender(
        sender: &VoteExecutor,
        endpoint: &AppendV2Endpoint,
        local: NodeId,
    ) {
        let campaign = sender.campaign(local).await.unwrap().unwrap();
        let identity = crate::load_or_create_node_identity(endpoint.source.as_ref()).unwrap();
        let state = sender.read_control_state().await.unwrap();
        let member = state.members.get(&local.to_string()).unwrap();
        let proof = crate::endpoint::peer_authentication_proof(
            endpoint,
            "tls://target:7443",
            &local.to_string(),
            &endpoint.target_id.to_string(),
        )
        .await
        .unwrap();
        let mut request = bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest {
            cluster_id: state.cluster_id,
            sender: local.as_bytes().to_vec(),
            recipient: endpoint.target_id.as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4(),
            term: campaign.term.0,
            last_log: campaign.last_log.map(wire_log_identity),
            proof: bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferProof {
                credential_serial: member.credential_serial.clone(),
                peer: proof,
                signature: Vec::new(),
            },
        };
        request.proof.signature =
            identity.sign(&crate::consensus_network::owned_vote_signing_payload(&request).unwrap());
        assert!(
            sender
                .request_campaign_vote(endpoint, local, request)
                .await
                .unwrap()
        );
        sender.initialize_leader().await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_v2_sender_provider_durable_and_signed_stale_term() {
        let root = tempfile::tempdir().unwrap();
        let (sender, endpoint, local) = append_v2_pair(root.path());
        elect_append_v2_sender(sender.as_ref(), &endpoint, local).await;
        sender
            .replicate_next(&endpoint, endpoint.target_id)
            .await
            .unwrap();
        let sent = sender.observe(local).await.unwrap();
        assert_eq!(
            endpoint
                .receiver
                .observe(endpoint.target_id)
                .await
                .unwrap()
                .last_log,
            sent.last_log
        );
        // Receiver is ahead in term; actual provider returns a signed stale-term
        // response, and the sender durably steps down instead of counting progress.
        endpoint
            .receiver
            .run(|gate| {
                gate.store
                    .persist(HardState {
                        term: Term(9),
                        voted_for: None,
                    })
                    .map_err(|_| VoteGateError::StorageFailure)
            })
            .await
            .unwrap();
        assert!(matches!(
            sender.replicate_next(&endpoint, endpoint.target_id).await,
            Err(bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError::StaleTerm)
        ));
        let after = sender.observe(local).await.unwrap();
        assert_eq!(after.hard_state.term, Term(9));
        assert!(!after.local_leader);
        assert_eq!(after.last_log, sent.last_log);
        assert_eq!(endpoint.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        sender.shutdown().await.unwrap();
        endpoint.receiver.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_v2_signed_higher_term_settles_despite_busy_and_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let (sender, endpoint, local) = append_v2_pair(root.path());
        elect_append_v2_sender(sender.as_ref(), &endpoint, local).await;
        endpoint
            .receiver
            .run(|gate| {
                gate.store
                    .persist(HardState {
                        term: Term(12),
                        voted_for: None,
                    })
                    .map_err(|_| VoteGateError::StorageFailure)
            })
            .await
            .unwrap();
        *endpoint.hold_sender.lock().unwrap() = Some(sender.clone());
        let endpoint = std::sync::Arc::new(endpoint);
        let owner = sender.clone();
        let remote = endpoint.clone();
        let call = tokio::spawn(async move {
            owner
                .replicate_next(remote.as_ref(), remote.target_id)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while sender
                .response_pending
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            sender.observe(local).await,
            Err(VoteGateError::Busy)
        ));
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        endpoint.held_guard.lock().unwrap().take();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while sender
                .response_pending
                .load(std::sync::atomic::Ordering::SeqCst)
                != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let after = sender.observe(local).await.unwrap();
        assert_eq!(after.hard_state.term, Term(12));
        assert!(!after.local_leader);
        sender.shutdown().await.unwrap();
        endpoint.receiver.shutdown().await.unwrap();
        drop(endpoint);
    }

    #[tokio::test]
    async fn append_v2_requires_selection_and_rejects_cross_version_signatures() {
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            members,
            mut request,
        } = authenticated_election_fixture();
        let local = *identity.node_id();
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_v2_signing_payload(&request).unwrap());
        assert!(matches!(
            executor
                .append_v2_authenticated(caller.clone(), local, request.clone())
                .await,
            Err(AuthenticatedAppendError::Unauthorized)
        ));
        assert!(matches!(
            executor
                .append_authenticated(caller.clone(), local, request.clone())
                .await,
            Err(AuthenticatedAppendError::Unauthorized)
        ));
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        assert!(matches!(
            executor
                .append_v2_authenticated(caller, local, request)
                .await,
            Err(AuthenticatedAppendError::Unauthorized)
        ));
        assert!(
            executor
                .observe(local)
                .await
                .unwrap()
                .election_activity
                .is_none()
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_v2_unselected_peer_is_incompatible_without_fallback() {
        let root = tempfile::tempdir().unwrap();
        let (sender, endpoint, local) = append_v2_pair(root.path());
        elect_append_v2_sender(sender.as_ref(), &endpoint, local).await;
        let peer = endpoint.target_id;
        sender
            .run(move |gate| {
                let mut state = gate.store.read_control_state().unwrap();
                state
                    .members
                    .get_mut(&peer.to_string())
                    .unwrap()
                    .negotiated_protocol
                    .features
                    .retain(|feature| {
                        feature
                            != bmux_cluster_plugin_api::cluster_consensus_rpc_v2::INTERFACE_ID
                                .as_str()
                    });
                let tx = gate.store.database.begin_write().unwrap();
                tx.open_table(TABLE)
                    .unwrap()
                    .insert("control", state.encode_snapshot().unwrap().as_slice())
                    .unwrap();
                tx.commit().unwrap();
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(
            sender.replicate_next(&endpoint, peer).await,
            Err(bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError::Incompatible)
        ));
        assert_eq!(endpoint.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        sender.shutdown().await.unwrap();
        endpoint.receiver.shutdown().await.unwrap();
    }

    fn fresh_election_proof(
        caller: &InstallCaller,
        members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
        identity: &crate::membership::NodeIdentity,
    ) -> bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferProof {
        let challenge = crate::membership::create_peer_auth_challenge_for_members(
            caller,
            &crate::ClusterPeerChallengeRequest {
                claimant_node_id: identity.node_id().to_string(),
            },
            members,
        )
        .unwrap();
        let mut peer = bmux_cluster_plugin_api::cluster_types::PeerAuthProof {
            challenge,
            claimant_node_id: identity.node_id().to_string(),
            claimant_credential_serial: members[0].credential_serial.clone(),
            claimant_signature: String::new(),
        };
        peer.claimant_signature = crate::membership::encode_hex(
            &identity.sign(&crate::membership::canonical_peer_proof(&peer).unwrap()),
        );
        bmux_cluster_plugin_api::cluster_consensus_transfer_types::TransferProof {
            credential_serial: members[0].credential_serial.clone(),
            signature: Vec::new(),
            peer,
        }
    }

    struct ElectionFixture {
        _root: tempfile::TempDir,
        executor: VoteExecutor,
        caller: std::sync::Arc<InstallCaller>,
        identity: crate::membership::NodeIdentity,
        members: Vec<bmux_cluster_plugin_api::cluster_types::ClusterMember>,
        request: bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest,
    }

    // Real credentials, challenge consumption, signatures and canonical product
    // membership exercise the remote boundary, not a synthetic auth shortcut.
    fn authenticated_election_fixture() -> ElectionFixture {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest as WireAppend;
        let caller = std::sync::Arc::new(InstallCaller::default());
        let identity = crate::membership::load_or_create_node_identity(caller.as_ref()).unwrap();
        crate::membership::initialize_cluster(caller.as_ref()).unwrap();
        let local = *identity.node_id();
        let mut members = crate::membership::list_members(caller.as_ref())
            .unwrap()
            .members;
        let member = &mut members[0];
        member.negotiated_protocol.features = vec![
            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID.to_string(),
        ];
        *member = crate::membership::issue_membership_credential(
            &identity,
            member.cluster_id.parse().unwrap(),
            member.node_id.clone(),
            member.public_key.clone(),
            member.capabilities.clone(),
            member.negotiated_protocol.clone(),
            crate::now_unix_ms(),
        )
        .unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut store = OwnedVoteStore::bootstrap(
            &root.path().join("authority"),
            &members[0].cluster_id,
            local,
            Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap(),
        )
        .unwrap();
        store.apply_committed().unwrap();
        let boundary = store.observe().unwrap().last_log.unwrap();
        let mut state = store.read_control_state().unwrap();
        state.members = members
            .iter()
            .cloned()
            .map(|member| (member.node_id.clone(), member))
            .collect();
        {
            let tx = store.database.begin_write().unwrap();
            tx.open_table(TABLE)
                .unwrap()
                .insert("control", state.encode_snapshot().unwrap().as_slice())
                .unwrap();
            tx.commit().unwrap();
        }
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        let mut request = WireAppend {
            cluster_id: members[0].cluster_id.clone(),
            sender: local.as_bytes().to_vec(),
            recipient: local.as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4(),
            term: boundary.term.0,
            previous: Some(wire_log_identity(boundary)),
            entries: Vec::new(),
            leader_commit: Some(wire_log_identity(boundary)),
            proof: fresh_election_proof(caller.as_ref(), &members, &identity),
        };
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        ElectionFixture {
            _root: root,
            executor,
            caller,
            identity,
            members,
            request,
        }
    }

    #[tokio::test]
    async fn authenticated_append_activity_rejects_spoof_replay_and_mismatch() {
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            members,
            mut request,
        } = authenticated_election_fixture();
        let local = *identity.node_id();
        let mut spoof = request.clone();
        spoof.proof.signature[0] ^= 1;
        assert!(
            executor
                .append_authenticated(caller.clone(), local, spoof)
                .await
                .is_err()
        );
        assert!(
            executor
                .observe(local)
                .await
                .unwrap()
                .election_activity
                .is_none()
        );
        let decision = executor
            .append_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert!(matches!(
            decision.outcome,
            Ok(AppendOutcome::Reconciled(
                AppendReconciliation::Matched { .. }
            ))
        ));
        let first = executor
            .observe(local)
            .await
            .unwrap()
            .election_activity
            .unwrap();
        assert_eq!(first.kind, ElectionActivityKind::AuthenticatedLeader);
        assert_eq!(first.peer, local);
        assert!(
            executor
                .append_authenticated(caller.clone(), local, request.clone())
                .await
                .is_err()
        );
        assert_eq!(
            executor.observe(local).await.unwrap().election_activity,
            Some(first)
        );

        // A valid leader can probe beyond this follower's divergent prefix.
        // Its durable mismatch response is activity even though no log is added.
        let deadline = executor.observe(local).await.unwrap();
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.previous.as_mut().unwrap().index += 10;
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        let decision = executor
            .append_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert!(matches!(
            decision.outcome,
            Ok(AppendOutcome::Reconciled(
                AppendReconciliation::PreviousMismatch
            ))
        ));
        let repaired = executor.observe(local).await.unwrap();
        let activity = repaired.election_activity.unwrap();
        assert_eq!(activity.sequence, first.sequence + 1);
        assert_eq!(repaired.hard_state, deadline.hard_state);
        assert_eq!(repaired.last_log, deadline.last_log);
        assert!(
            executor
                .campaign_if_unchanged(
                    local,
                    deadline.hard_state,
                    deadline.effective_membership.unwrap(),
                    Some(first.sequence),
                )
                .await
                .unwrap()
                .is_none()
        );
        executor.shutdown().await.unwrap();
    }

    async fn check_committed_activity_contradiction(
        compacted: bool,
        mismatch: bool,
        advertised_term: u64,
    ) {
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            members,
            mut request,
        } = authenticated_election_fixture();
        let local = *identity.node_id();
        let before = executor.observe(local).await.unwrap();
        let boundary = LogIdentity {
            term: Term(2),
            leader: local,
            index: crate::consensus_model::LogIndex(5),
        };
        executor
            .append(
                AppendRequest {
                    term: Term(2),
                    leader: local,
                    previous: before.last_log,
                    entries: (1..=5)
                        .map(|index| Entry {
                            identity: LogIdentity {
                                index: crate::consensus_model::LogIndex(index),
                                ..boundary
                            },
                            payload: EntryPayload::NoOp,
                        })
                        .collect(),
                    leader_commit: Some(boundary),
                },
                (5, 1024),
            )
            .await
            .unwrap();
        executor.apply_committed().await.unwrap();
        if compacted {
            executor.publish_snapshot().await.unwrap();
            executor.compact_snapshot().await.unwrap();
        }
        let advertised = LogIdentity {
            term: Term(advertised_term),
            leader: if advertised_term == 2 {
                NodeId::from(2)
            } else {
                local
            },
            ..boundary
        };
        request.term = 4;
        request.previous = Some(wire_log_identity(if mismatch {
            LogIdentity {
                term: Term(3),
                index: crate::consensus_model::LogIndex(10),
                ..boundary
            }
        } else {
            boundary
        }));
        request.leader_commit = Some(wire_log_identity(advertised));
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        let result = executor.append_authenticated(caller, local, request).await;
        if mismatch {
            assert!(
                matches!(
                    result,
                    Ok(AppendDecision {
                        outcome: Ok(AppendOutcome::Reconciled(
                            AppendReconciliation::PreviousMismatch
                        )),
                        ..
                    })
                ),
                "compacted={compacted} mismatch={mismatch} advertised_term={advertised_term}: {result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(AuthenticatedAppendError::Invalid(_))),
                "compacted={compacted} mismatch={mismatch} advertised_term={advertised_term}: {result:?}"
            );
        }
        let after = executor.observe(local).await.unwrap();
        assert_eq!(after.hard_state.term, Term(4));
        assert_eq!(after.committed, Some(boundary));
        assert_eq!(after.last_log, Some(boundary));
        assert!(after.election_activity.is_none());
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn committed_and_snapshot_commit_identity_conflicts_never_refresh_activity() {
        for compacted in [false, true] {
            for mismatch in [false, true] {
                for advertised_term in [1, 2, 3] {
                    check_committed_activity_contradiction(compacted, mismatch, advertised_term)
                        .await;
                }
            }
        }
    }

    #[tokio::test]
    async fn contradictory_advertised_commit_never_refreshes_activity() {
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            members,
            mut request,
        } = authenticated_election_fixture();
        let local = *identity.node_id();
        let boundary =
            crate::consensus_network::transfer_position(request.previous.as_ref().unwrap())
                .unwrap();
        let other = NodeId::from(2);
        // Cover both matching and missing predecessors; the advertised commit
        // is beyond either request prefix, so reconciliation alone misses it.
        for mismatch in [false, true] {
            request.term = boundary.term.0 + 1;
            request.previous = Some(wire_log_identity(LogIdentity {
                index: crate::consensus_model::LogIndex(
                    boundary.index.0 + if mismatch { 10 } else { 0 },
                ),
                ..boundary
            }));
            request.leader_commit = Some(wire_log_identity(LogIdentity {
                term: Term(request.term),
                leader: other,
                index: crate::consensus_model::LogIndex(boundary.index.0 + 20),
            }));
            request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
            request.proof.signature = identity
                .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
            let decision = executor
                .append_authenticated(caller.clone(), local, request.clone())
                .await
                .unwrap();
            assert!(matches!(decision.outcome, Ok(AppendOutcome::Reconciled(_))));
            let after = executor.observe(local).await.unwrap();
            assert_eq!(after.hard_state.term, Term(request.term));
            assert!(after.election_activity.is_none());
        }
        // An older-term commit cannot contradict an equal-term identity already
        // retained locally, even when its predecessor is a healthy mismatch.
        request.previous.as_mut().unwrap().term = request.term;
        request.leader_commit = Some(wire_log_identity(LogIdentity {
            leader: other,
            ..boundary
        }));
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        let decision = executor
            .append_authenticated(caller, local, request)
            .await
            .unwrap();
        assert!(matches!(
            decision.outcome,
            Ok(AppendOutcome::Reconciled(
                AppendReconciliation::PreviousMismatch
            ))
        ));
        assert!(
            executor
                .observe(local)
                .await
                .unwrap()
                .election_activity
                .is_none()
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_suffix_cannot_turn_prefix_mismatch_into_election_activity() {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendEntry;
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            members,
            mut request,
        } = authenticated_election_fixture();
        let local = *identity.node_id();
        let boundary =
            crate::consensus_network::transfer_position(request.previous.as_ref().unwrap())
                .unwrap();
        let before = executor.observe(local).await.unwrap();
        // Reusing the committed identity with different payload is equivocation.
        request.previous = None;
        request.entries = vec![AppendEntry::Inline {
            canonical: crate::consensus_storage::encode_owned_entry(&Entry {
                identity: boundary,
                payload: EntryPayload::Application(b"different payload".to_vec()),
            })
            .unwrap(),
        }];
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        let decision = executor
            .append_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert!(decision.outcome.is_err());
        assert_eq!(
            executor.observe(local).await.unwrap().election_activity,
            before.election_activity
        );
        // Even with a missing predecessor, a direct uniform voter-set change
        // is invalid. A PreviousMismatch must not conceal this admission error.
        let previous = LogIdentity {
            index: crate::consensus_model::LogIndex(boundary.index.0 + 10),
            ..boundary
        };
        request.previous = Some(wire_log_identity(previous));
        let other = NodeId::from(2);
        request.entries = vec![AppendEntry::Inline {
            canonical: crate::consensus_storage::encode_owned_entry(&Entry {
                identity: LogIdentity {
                    index: crate::consensus_model::LogIndex(previous.index.0 + 1),
                    ..boundary
                },
                payload: EntryPayload::Membership(
                    Membership::new(
                        vec![vec![local, other]],
                        vec![(local, String::new()), (other, String::new())],
                    )
                    .unwrap(),
                ),
            })
            .unwrap(),
        }];
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        let decision = executor
            .append_authenticated(caller, local, request)
            .await
            .unwrap();
        assert!(matches!(
            decision.outcome,
            Ok(AppendOutcome::Reconciled(
                AppendReconciliation::PreviousMismatch
            ))
        ));
        let after = executor.observe(local).await.unwrap();
        assert_eq!(after.election_activity, before.election_activity);
        assert_eq!(after.hard_state, before.hard_state);
        assert!(
            executor
                .campaign_if_unchanged(
                    local,
                    before.hard_state,
                    before.effective_membership.unwrap(),
                    None,
                )
                .await
                .unwrap()
                .is_some()
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_vote_activity_and_stale_malformed_append() {
        let ElectionFixture {
            _root,
            executor,
            caller,
            identity,
            members,
            mut request,
        } = authenticated_election_fixture();
        let local = *identity.node_id();
        let boundary =
            crate::consensus_network::transfer_position(request.previous.as_ref().unwrap())
                .unwrap();
        let before_vote = std::time::Instant::now();
        // A granted authenticated vote publishes only after durable term/vote.
        let mut vote = bmux_cluster_plugin_api::cluster_consensus_vote_types::VoteRequest {
            cluster_id: members[0].cluster_id.clone(),
            sender: local.as_bytes().to_vec(),
            recipient: local.as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4(),
            term: boundary.term.0 + 1,
            last_log: Some(wire_log_identity(boundary)),
            proof: fresh_election_proof(caller.as_ref(), &members, &identity),
        };
        vote.proof.signature =
            identity.sign(&crate::consensus_network::owned_vote_signing_payload(&vote).unwrap());
        assert!(
            executor
                .vote_authenticated(caller.clone(), local, vote)
                .await
                .unwrap()
                .granted
        );
        let granted = executor.observe(local).await.unwrap();
        let activity = granted.election_activity.unwrap();
        assert_eq!(activity.kind, ElectionActivityKind::GrantedVote);
        assert_eq!(activity.sequence, 1);
        assert_eq!(granted.hard_state.voted_for, Some(local));
        assert_eq!(granted.hard_state.term, activity.term);
        assert!(activity.observed_at >= before_vote);

        request.previous = Some(wire_log_identity(boundary));
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        assert!(
            executor
                .append_authenticated(caller.clone(), local, request.clone())
                .await
                .is_err()
        );
        assert_eq!(
            executor.observe(local).await.unwrap().election_activity,
            Some(activity)
        );
        request.entries.push(
            bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendEntry::Inline {
                canonical: b"malformed".to_vec(),
            },
        );
        request.term = activity.term.0;
        request.proof = fresh_election_proof(caller.as_ref(), &members, &identity);
        request.proof.signature = identity
            .sign(&crate::consensus_network::owned_append_signing_payload(&request).unwrap());
        assert!(
            executor
                .append_authenticated(caller, local, request)
                .await
                .is_err()
        );
        let after = executor.observe(local).await.unwrap();
        assert_eq!(after.election_activity, Some(activity));
        assert!(
            executor
                .campaign_if_unchanged(
                    local,
                    granted.hard_state,
                    granted.effective_membership.unwrap(),
                    Some(activity.sequence),
                )
                .await
                .unwrap()
                .is_some()
        );
        executor.shutdown().await.unwrap();
    }

    fn fresh_install_proof(
        caller: &InstallCaller,
        members: &[bmux_cluster_plugin_api::cluster_types::ClusterMember],
        identity: &crate::membership::NodeIdentity,
        request: &mut bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
    ) {
        let challenge = crate::membership::create_peer_auth_challenge_for_members(
            caller,
            &crate::ClusterPeerChallengeRequest {
                claimant_node_id: identity.node_id().to_string(),
            },
            members,
        )
        .unwrap();
        let mut proof = request.proof.peer.clone();
        proof.challenge = challenge;
        proof.claimant_signature = crate::membership::encode_hex(
            &identity.sign(&crate::membership::canonical_peer_proof(&proof).unwrap()),
        );
        request.proof.peer = proof;
        request.proof.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_signing_payload(request).unwrap(),
        );
    }

    fn assert_install_replay(
        request: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallRequest,
        actual: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallResponse,
        original: &bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallResponse,
    ) {
        crate::consensus_network::verify_owned_snapshot_install_response(request, actual).unwrap();
        assert_eq!(actual.request_id, original.request_id);
        assert_eq!(actual.source, original.source);
        assert_eq!(actual.term, original.term);
        assert_eq!(actual.outcome, original.outcome);
        // Fresh peer-auth proofs intentionally yield different response signatures.
    }

    // Exercise the actual authenticated executor, sealed reader and durable
    // replay transaction together, without a network/runtime surrogate store.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn remote_snapshot_signed_sealed_install_replay_reopen_and_conflict() {
        use bmux_cluster_plugin_api::{
            cluster_consensus_snapshot_types::{InstallOutcome, InstallRejection, InstallRequest},
            cluster_consensus_transfer_types::{
                NodeAddress, SnapshotDescriptor, StoredMembership, TransferHeader, TransferProof,
                TransferPurpose, VoterConfiguration,
            },
            cluster_types::PeerAuthProof,
        };
        let caller = std::sync::Arc::new(InstallCaller::default());
        let identity = crate::membership::load_or_create_node_identity(caller.as_ref()).unwrap();
        crate::membership::initialize_cluster(caller.as_ref()).unwrap();
        let local = *identity.node_id();
        let mut members = crate::membership::list_members(caller.as_ref())
            .unwrap()
            .members;
        let member = members
            .iter_mut()
            .find(|m| m.node_id == local.to_string())
            .unwrap();
        member.negotiated_protocol.features = vec![
            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_snapshot::INTERFACE_ID.to_string(),
        ];
        *member = crate::membership::issue_membership_credential(
            &identity,
            member.cluster_id.parse().unwrap(),
            member.node_id.clone(),
            member.public_key.clone(),
            member.capabilities.clone(),
            member.negotiated_protocol.clone(),
            crate::now_unix_ms(),
        )
        .unwrap();
        let cluster = member.cluster_id.clone();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let mut store = OwnedVoteStore::bootstrap(
            &path,
            &cluster,
            local,
            Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap(),
        )
        .unwrap();
        store.apply_committed().unwrap();
        let mut state = store.read_control_state().unwrap();
        state.members = members
            .iter()
            .cloned()
            .map(|m| (m.node_id.clone(), m))
            .collect();
        {
            let tx = store.database.begin_write().unwrap();
            tx.open_table(TABLE)
                .unwrap()
                .insert("control", state.encode_snapshot().unwrap().as_slice())
                .unwrap();
            tx.commit().unwrap();
        }
        let bytes = store.publish_snapshot().unwrap();
        let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(&bytes).unwrap();
        let boundary = descriptor.last_included.unwrap();
        let introduced = descriptor
            .membership
            .as_ref()
            .unwrap()
            .introduced_at
            .unwrap();
        let challenge = crate::membership::create_peer_auth_challenge_for_members(
            caller.as_ref(),
            &crate::ClusterPeerChallengeRequest {
                claimant_node_id: local.to_string(),
            },
            &members,
        )
        .unwrap();
        let mut proof = PeerAuthProof {
            challenge,
            claimant_node_id: local.to_string(),
            claimant_credential_serial: members[0].credential_serial.clone(),
            claimant_signature: String::new(),
        };
        proof.claimant_signature = crate::membership::encode_hex(
            &identity.sign(&crate::membership::canonical_peer_proof(&proof).unwrap()),
        );
        let mut request = InstallRequest {
            request_id: uuid::Uuid::new_v4(),
            reference: TransferHeader {
                cluster_id: cluster.clone(),
                sender: local.as_bytes().to_vec(),
                recipient: local.as_bytes().to_vec(),
                leader_term: 0,
                sender_incarnation: uuid::Uuid::new_v4(),
                sequence: 1,
                purpose: TransferPurpose::Snapshot {
                    value: SnapshotDescriptor {
                        format_version: descriptor.format_version,
                        cluster_id: cluster.clone(),
                        snapshot_id: descriptor.snapshot_id,
                        last_included: Some(wire_log_identity(boundary)),
                        membership: Some(StoredMembership {
                            introduced_at: wire_log_identity(introduced),
                            configurations: vec![VoterConfiguration {
                                voters: vec![local.as_bytes().to_vec()],
                            }],
                            nodes: vec![NodeAddress {
                                node: local.as_bytes().to_vec(),
                                endpoint: String::new(),
                            }],
                        }),
                        total_bytes: bytes.len() as u64,
                        checksum: descriptor.checksum.to_vec(),
                    },
                },
                total_bytes: bytes.len() as u64,
                checksum: Sha256::digest(&bytes).to_vec(),
            },
            proof: TransferProof {
                credential_serial: members[0].credential_serial.clone(),
                peer: proof,
                signature: Vec::new(),
            },
        };
        request.proof.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_signing_payload(&request).unwrap(),
        );
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        let rejected = executor
            .install_snapshot_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert!(matches!(
            rejected.outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::UnknownTransfer
            }
        ));
        assert!(matches!(executor.install_snapshot_authenticated(caller.clone(), local, request.clone()).await,
            Err(bmux_cluster_plugin_api::cluster_consensus_snapshot_types::InstallError::Unauthorized)));
        let directory = std::fs::File::open(root.path()).unwrap();
        let binding: [u8; 32] =
            Sha256::digest(bmux_plugin_sdk::encode_service_message(&request.reference).unwrap())
                .into();
        let key = crate::transfer_staging::TransferKey {
            peer: *local.as_bytes(),
            incarnation: request.reference.sender_incarnation,
            sequence: 1,
        };
        let staged = bytes.clone();
        executor
            .run(move |gate| {
                let mut staging =
                    crate::transfer_staging::TransferStaging::new(1024 * 1024, 1024 * 1024)
                        .unwrap();
                let now = std::time::Instant::now();
                staging
                    .begin(
                        &directory,
                        key,
                        binding,
                        staged.len() as u64,
                        Sha256::digest(&staged).into(),
                        now,
                    )
                    .unwrap();
                for (offset, chunk) in staged.chunks(64 * 1024).enumerate() {
                    staging
                        .chunk(key, (offset * 64 * 1024) as u64, chunk, now)
                        .unwrap();
                }
                gate.staging = Some((directory, staging));
                Ok(())
            })
            .await
            .unwrap();
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut request);
        assert!(matches!(
            executor
                .install_snapshot_authenticated(caller.clone(), local, request.clone())
                .await
                .unwrap()
                .outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::NotSealed
            }
        ));
        executor
            .run(move |gate| {
                gate.staging.as_mut().unwrap().1.finish(key).unwrap();
                Ok(())
            })
            .await
            .unwrap();
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut request);
        let installed = executor
            .install_snapshot_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert!(matches!(
            installed.outcome,
            InstallOutcome::Installed { .. }
        ));
        crate::consensus_network::verify_owned_snapshot_install_response(&request, &installed)
            .unwrap();
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut request);
        let replay = executor
            .install_snapshot_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert_install_replay(&request, &replay, &installed);
        executor.shutdown().await.unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(
            2,
            OwnedVoteStore::open(&path, &cluster).unwrap(),
        ));
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut request);
        let replay = executor
            .install_snapshot_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert_install_replay(&request, &replay, &installed);
        let mut conflict = request.clone();
        conflict.reference.sequence += 1;
        conflict.proof.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_signing_payload(&conflict).unwrap(),
        );
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut conflict);
        assert!(matches!(
            executor
                .install_snapshot_authenticated(caller.clone(), local, conflict)
                .await
                .unwrap()
                .outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::ConflictingReference
            }
        ));
        let mut generation = request.clone();
        generation.reference.sender_incarnation = uuid::Uuid::new_v4();
        generation.proof.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_signing_payload(&generation).unwrap(),
        );
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut generation);
        assert!(matches!(
            executor
                .install_snapshot_authenticated(caller.clone(), local, generation)
                .await
                .unwrap()
                .outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::ConflictingReference
            }
        ));
        let mut capacity = request.clone();
        capacity.request_id = uuid::Uuid::new_v4();
        capacity.reference.sequence = 8;
        capacity.reference.total_bytes = 24 * 1024 * 1024;
        if let TransferPurpose::Snapshot { value } = &mut capacity.reference.purpose {
            value.total_bytes = capacity.reference.total_bytes;
        }
        capacity.proof.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_signing_payload(&capacity).unwrap(),
        );
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut capacity);
        assert!(matches!(
            executor
                .install_snapshot_authenticated(caller.clone(), local, capacity)
                .await
                .unwrap()
                .outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::CapacityUnavailable
            }
        ));
        executor
            .run(|gate| {
                gate.store
                    .persist(HardState {
                        term: Term(1),
                        voted_for: None,
                    })
                    .map_err(|_| VoteGateError::StorageFailure)
            })
            .await
            .unwrap();
        let mut stale_request = request.clone();
        stale_request.request_id = uuid::Uuid::new_v4();
        stale_request.reference.sequence = 9;
        stale_request.proof.signature = identity.sign(
            &crate::consensus_network::owned_snapshot_install_signing_payload(&stale_request)
                .unwrap(),
        );
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut stale_request);
        assert!(matches!(
            executor
                .install_snapshot_authenticated(caller.clone(), local, stale_request)
                .await
                .unwrap()
                .outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::StaleTerm
            }
        ));
        // Receipt replay stays durable after a term advance; fresh authorization
        // is still mandatory and revocation cannot revive old install authority.
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut request);
        let replay = executor
            .install_snapshot_authenticated(caller.clone(), local, request.clone())
            .await
            .unwrap();
        assert_install_replay(&request, &replay, &installed);
        // A conflicting reuse must still settle authenticated higher authority
        // before reporting conflict, cancelling old leadership/read admission.
        executor
            .run(move |gate| {
                let observation = gate.store.observe()?;
                let membership = observation.effective_membership.unwrap();
                gate.store
                    .persist(HardState {
                        term: Term(1),
                        voted_for: Some(local),
                    })
                    .map_err(|_| VoteGateError::StorageFailure)?;
                let entry = LogIdentity {
                    term: Term(1),
                    leader: local,
                    index: crate::consensus_model::LogIndex(1),
                };
                gate.store
                    .append(
                        &AppendRequest {
                            term: Term(1),
                            leader: local,
                            previous: observation.last_log,
                            entries: vec![Entry {
                                identity: entry,
                                payload: EntryPayload::NoOp,
                            }],
                            leader_commit: Some(entry),
                        },
                        (1, 1024),
                    )
                    .map_err(|_| VoteGateError::StorageFailure)?;
                gate.store
                    .apply_committed()
                    .map_err(|_| VoteGateError::StorageFailure)?;
                gate.leader = Some(
                    crate::consensus_model::LeaderProgress::after_durable_initialization(
                        membership.membership,
                        entry,
                    ),
                );
                gate.initialized_noop = Some(entry);
                Ok(())
            })
            .await
            .unwrap();
        let context = executor.begin_read(local).await.unwrap().unwrap();
        let mut higher_conflict = request.clone();
        higher_conflict.reference.leader_term = 2;
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut higher_conflict);
        let response = executor
            .install_snapshot_authenticated(caller.clone(), local, higher_conflict)
            .await
            .unwrap();
        assert!(matches!(
            response.outcome,
            InstallOutcome::Rejected {
                reason: InstallRejection::ConflictingReference
            }
        ));
        assert_eq!(
            executor.observe(local).await.unwrap().hard_state,
            HardState {
                term: Term(2),
                voted_for: None
            }
        );
        assert!(!executor.observe(local).await.unwrap().local_leader);
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        executor
            .run(|gate| {
                assert!(gate.campaign.is_none());
                assert!(gate.initialized_noop.is_none());
                Ok(())
            })
            .await
            .unwrap();
        // Issue while the committed member is active, then revoke canonically:
        // rejection must be current membership auth, not reused challenge auth.
        fresh_install_proof(caller.as_ref(), &members, &identity, &mut request);
        let saved = executor
            .read_control_state()
            .await
            .unwrap()
            .encode_snapshot()
            .unwrap();
        executor
            .run(move |gate| {
                let mut state = gate.store.read_control_state().unwrap();
                state.members.get_mut(&local.to_string()).unwrap().state =
                    bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Revoked;
                let tx = gate.store.database.begin_write().unwrap();
                tx.open_table(TABLE)
                    .unwrap()
                    .insert("control", state.encode_snapshot().unwrap().as_slice())
                    .unwrap();
                tx.commit().unwrap();
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            executor
                .install_snapshot_authenticated(caller, local, request)
                .await
                .is_err()
        );
        executor
            .run(move |gate| {
                let tx = gate.store.database.begin_write().unwrap();
                tx.open_table(TABLE)
                    .unwrap()
                    .insert("control", saved.as_slice())
                    .unwrap();
                tx.commit().unwrap();
                Ok(())
            })
            .await
            .unwrap();
        executor.shutdown().await.unwrap();
        assert_eq!(
            OwnedVoteStore::open(&path, &cluster).unwrap().state(),
            HardState {
                term: Term(2),
                voted_for: None
            }
        );
    }

    fn read_fixture_genesis(membership: &Membership) -> Membership {
        if membership.configurations().len() != 2 {
            return membership.clone();
        }
        Membership::new(
            vec![membership.configurations()[0].iter().copied().collect()],
            membership
                .nodes()
                .iter()
                .map(|(node, endpoint)| (*node, endpoint.clone()))
                .collect(),
        )
        .unwrap()
    }

    fn read_barrier_fixture(
        path: &Path,
        configurations: Vec<Vec<NodeId>>,
        apply: bool,
    ) -> (VoteExecutor, NodeId, LogIdentity) {
        let local = NodeId::from(1);
        let nodes = configurations
            .iter()
            .flatten()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let membership = Membership::new(
            configurations,
            nodes
                .into_iter()
                .map(|node| (node, String::new()))
                .collect(),
        )
        .unwrap();
        let genesis = LogIdentity {
            term: Term(0),
            leader: local,
            index: crate::consensus_model::LogIndex(0),
        };
        let mut store = OwnedVoteStore::initialize(
            path,
            "cluster",
            Some(Entry {
                identity: genesis,
                payload: EntryPayload::Membership(read_fixture_genesis(&membership)),
            }),
        )
        .unwrap();
        store.apply_committed().unwrap();
        let previous = if membership.configurations().len() == 2 {
            let joint = LogIdentity {
                term: Term(1),
                index: crate::consensus_model::LogIndex(1),
                ..genesis
            };
            store
                .append(
                    &AppendRequest {
                        term: Term(1),
                        leader: local,
                        previous: Some(genesis),
                        entries: vec![Entry {
                            identity: joint,
                            payload: EntryPayload::Membership(membership.clone()),
                        }],
                        leader_commit: Some(joint),
                    },
                    (1, 4096),
                )
                .unwrap();
            store.apply_committed().unwrap();
            joint
        } else {
            genesis
        };
        let noop = LogIdentity {
            term: Term(1),
            index: crate::consensus_model::LogIndex(previous.index.0 + 1),
            ..genesis
        };
        store
            .append(
                &AppendRequest {
                    term: Term(1),
                    leader: local,
                    previous: Some(previous),
                    entries: vec![Entry {
                        identity: noop,
                        payload: EntryPayload::NoOp,
                    }],
                    leader_commit: Some(noop),
                },
                (1, 1024),
            )
            .unwrap();
        store
            .persist(HardState {
                term: Term(1),
                voted_for: Some(local),
            })
            .unwrap();
        if apply {
            store.apply_committed().unwrap();
        }
        let mut gate = DurableVoteGate::new(1, store);
        gate.leader = Some(
            crate::consensus_model::LeaderProgress::after_durable_initialization(membership, noop),
        );
        gate.initialized_noop = Some(noop);
        (VoteExecutor::new(gate), local, noop)
    }

    async fn acknowledge_read(
        executor: &VoteExecutor,
        context: crate::consensus_model::ReadContext,
        peer: NodeId,
        term: Term,
        accepted: bool,
    ) {
        executor
            .run(move |gate| {
                gate.observe_read_confirmation(
                    peer,
                    crate::consensus_model::ReadConfirmation {
                        context,
                        term,
                        accepted,
                    },
                )
            })
            .await
            .unwrap();
    }

    #[test]
    fn owned_read_signed_heartbeat_reply_cannot_bind_another_request() {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::{
            AppendOutcome as WireOutcome, AppendResponse,
        };
        let local = NodeId::from(1);
        let identity = crate::membership::NodeIdentity::new_for_test(2);
        let peer = *identity.node_id();
        let mut request = append_admission_request(local, peer, "cluster".into());
        let mut response = AppendResponse {
            request_id: request.request_id,
            source: peer.as_bytes().to_vec(),
            term: request.term,
            outcome: WireOutcome::Durable {
                matched: request.previous.clone(),
            },
            signature: Vec::new(),
        };
        response.signature = identity.sign(
            &crate::consensus_network::owned_append_response_signing_payload(&request, &response)
                .unwrap(),
        );
        crate::consensus_network::verify_owned_append_response(&request, &response).unwrap();
        request.request_id = uuid::Uuid::new_v4();
        assert!(
            crate::consensus_network::verify_owned_append_response(&request, &response).is_err()
        );
        request.request_id = response.request_id;
        request.leader_commit = None;
        assert!(
            crate::consensus_network::verify_owned_append_response(&request, &response).is_err()
        );
    }

    #[tokio::test]
    async fn owned_read_single_voter_applied_lag_stale_and_once() {
        let root = tempfile::tempdir().unwrap();
        let (executor, local, noop) = read_barrier_fixture(
            &root.path().join("authority"),
            vec![vec![NodeId::from(1)]],
            false,
        );
        let stale = executor.begin_read(local).await.unwrap().unwrap();
        let context = executor.begin_read(local).await.unwrap().unwrap();
        assert_ne!(stale, context);
        assert!(
            executor
                .complete_read(local, stale)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        executor.apply_committed_batch().await.unwrap();
        let result = executor
            .complete_read(local, context)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.watermark, noop);
        assert_eq!(result.state.revision, 0);
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn owned_read_joint_quorum_requires_both_majorities() {
        let root = tempfile::tempdir().unwrap();
        let nodes = [
            NodeId::from(1),
            NodeId::from(2),
            NodeId::from(3),
            NodeId::from(4),
            NodeId::from(5),
        ];
        let (executor, local, noop) = read_barrier_fixture(
            &root.path().join("authority"),
            vec![
                vec![nodes[0], nodes[1], nodes[2]],
                vec![nodes[0], nodes[3], nodes[4]],
            ],
            true,
        );
        let stale = executor.begin_read(local).await.unwrap().unwrap();
        let context = executor.begin_read(local).await.unwrap().unwrap();
        acknowledge_read(&executor, stale, nodes[1], context.term, true).await;
        acknowledge_read(&executor, context, nodes[3], context.term, false).await;
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        acknowledge_read(&executor, context, nodes[1], context.term, true).await;
        acknowledge_read(&executor, context, nodes[2], context.term, true).await;
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        acknowledge_read(&executor, context, nodes[3], context.term, true).await;
        assert_eq!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .unwrap()
                .watermark,
            noop
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn owned_read_higher_term_persists_even_for_stale_context() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (executor, local, _) = read_barrier_fixture(
            &path,
            vec![vec![NodeId::from(1), NodeId::from(2), NodeId::from(3)]],
            true,
        );
        let stale = executor.begin_read(local).await.unwrap().unwrap();
        let current = executor.begin_read(local).await.unwrap().unwrap();
        acknowledge_read(&executor, stale, NodeId::from(2), Term(2), false).await;
        assert!(
            executor
                .complete_read(local, current)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!executor.observe(local).await.unwrap().local_leader);
        executor.shutdown().await.unwrap();
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(
            store.state(),
            HardState {
                term: Term(2),
                voted_for: None
            }
        );
    }

    #[tokio::test]
    async fn owned_read_verified_higher_term_settlement_survives_cancelled_waiter() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (executor, local, _) = read_barrier_fixture(
            &path,
            vec![vec![NodeId::from(1), NodeId::from(2), NodeId::from(3)]],
            true,
        );
        let executor = std::sync::Arc::new(executor);
        let context = executor.begin_read(local).await.unwrap().unwrap();
        let guard = executor.gate.clone().lock_owned().await;
        let owner = executor.clone();
        let permit = executor.outbound_votes.clone().try_acquire_owned().unwrap();
        let task = tokio::spawn(async move {
            owner
                .settle_response(permit, move |gate| {
                    gate.observe_read_confirmation(
                        NodeId::from(2),
                        crate::consensus_model::ReadConfirmation {
                            context,
                            term: Term(2),
                            accepted: false,
                        },
                    )
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while executor
                .response_pending
                .load(std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            executor.begin_read(local).await,
            Err(VoteGateError::Busy)
        ));
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while executor
                .response_pending
                .load(std::sync::atomic::Ordering::SeqCst)
                != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            executor.observe(local).await.unwrap().hard_state.term,
            Term(2)
        );
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        executor.shutdown().await.unwrap();
        assert_eq!(
            OwnedVoteStore::open(&path, "cluster").unwrap().state().term,
            Term(2)
        );
    }

    // Both production response boundaries must retain their already-reserved
    // task through durable settlement, even after the requesting task is gone.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn verified_vote_and_append_higher_terms_survive_busy_cancelled_waiters() {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types as wire, cluster_consensus_vote_types as vote,
        };
        for append in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("authority");
            let (executor, local, noop) = read_barrier_fixture(
                &path,
                vec![vec![NodeId::from(1), NodeId::from(2), NodeId::from(3)]],
                true,
            );
            let executor = std::sync::Arc::new(executor);
            let context = executor.begin_read(local).await.unwrap().unwrap();
            let previous = executor.observe(local).await.unwrap().hard_state;
            let remote = crate::membership::NodeIdentity::new_for_test(2);
            let peer = *remote.node_id();
            let mut request = append_admission_request(local, peer, "cluster".into());
            // Deliberately stale probe: persistence must precede its rejection.
            request.term = 0;
            let permit = executor.outbound_votes.clone().try_acquire_owned().unwrap();
            let guard = executor.gate.clone().lock_owned().await;
            let owner = executor.clone();
            let task = tokio::spawn(async move {
                if append {
                    let mut response = wire::AppendResponse {
                        request_id: request.request_id,
                        source: peer.as_bytes().to_vec(),
                        term: 2,
                        outcome: wire::AppendOutcome::Conflict { next_index: 0 },
                        signature: Vec::new(),
                    };
                    response.signature = remote.sign(
                        &crate::consensus_network::owned_append_response_signing_payload(
                            &request, &response,
                        )
                        .unwrap(),
                    );
                    crate::consensus_network::verify_owned_append_response(&request, &response)
                        .unwrap();
                    owner
                        .replicate_response_admitted(
                            local,
                            request,
                            response,
                            "obsolete".into(),
                            peer,
                            Some(noop),
                            Membership::new(vec![vec![local]], vec![(local, String::new())])
                                .unwrap(),
                            permit,
                            false,
                        )
                        .await
                        .unwrap_err();
                } else {
                    let request = vote::VoteRequest {
                        cluster_id: request.cluster_id,
                        sender: request.sender,
                        recipient: request.recipient,
                        request_id: request.request_id,
                        term: request.term,
                        last_log: request.previous,
                        proof: request.proof,
                    };
                    let mut response = vote::VoteResponse {
                        request_id: request.request_id,
                        source: peer.as_bytes().to_vec(),
                        term: 2,
                        granted: false,
                        signature: Vec::new(),
                    };
                    response.signature = remote.sign(
                        &crate::consensus_network::owned_vote_response_signing_payload(
                            &request, &response,
                        )
                        .unwrap(),
                    );
                    owner
                        .campaign_signed_response_admitted(local, &request, &response, permit)
                        .await
                        .unwrap();
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while executor
                    .response_pending
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(matches!(
                executor.begin_read(local).await,
                Err(VoteGateError::Busy)
            ));
            assert!(matches!(
                executor
                    .propose_authorized(
                        local,
                        previous,
                        "principal:test".into(),
                        proposal_command(),
                        |_, _| Ok(())
                    )
                    .await,
                Err(ProposalError::Executor(VoteGateError::Busy))
            ));
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            drop(guard);
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while executor
                    .response_pending
                    .load(std::sync::atomic::Ordering::SeqCst)
                    != 0
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                executor.observe(local).await.unwrap().hard_state,
                HardState {
                    term: Term(2),
                    voted_for: None
                }
            );
            assert!(!executor.observe(local).await.unwrap().local_leader);
            assert!(
                executor
                    .complete_read(local, context)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(executor.begin_read(local).await.unwrap().is_none());
            assert!(matches!(
                executor
                    .propose_authorized(
                        local,
                        previous,
                        "principal:test".into(),
                        proposal_command(),
                        |_, _| Ok(())
                    )
                    .await,
                Err(ProposalError::StaleAuthority)
            ));
            executor
                .run(|gate| {
                    assert!(gate.campaign.is_none());
                    assert!(gate.initialized_noop.is_none());
                    Ok(())
                })
                .await
                .unwrap();
            executor.shutdown().await.unwrap();
            assert_eq!(
                OwnedVoteStore::open(&path, "cluster").unwrap().state(),
                HardState {
                    term: Term(2),
                    voted_for: None
                }
            );
        }
    }

    #[tokio::test]
    async fn owned_read_membership_publication_cancels_even_after_restoration() {
        let root = tempfile::tempdir().unwrap();
        let (executor, local, noop) = read_barrier_fixture(
            &root.path().join("authority"),
            vec![vec![NodeId::from(1)]],
            true,
        );
        let context = executor.begin_read(local).await.unwrap().unwrap();
        executor
            .run(move |gate| {
                let membership = gate.leader.as_ref().unwrap().membership.clone();
                let identity = LogIdentity {
                    index: crate::consensus_model::LogIndex(2),
                    ..noop
                };
                gate.store
                    .append(
                        &AppendRequest {
                            term: noop.term,
                            leader: local,
                            previous: Some(noop),
                            entries: vec![Entry {
                                identity,
                                payload: EntryPayload::Membership(membership),
                            }],
                            leader_commit: Some(noop),
                        },
                        (1, 1024),
                    )
                    .map_err(|_| VoteGateError::StorageFailure)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        executor
            .run(move |gate| {
                gate.store
                    .append(
                        &AppendRequest {
                            term: Term(2),
                            leader: local,
                            previous: Some(noop),
                            entries: vec![Entry {
                                identity: LogIdentity {
                                    term: Term(2),
                                    index: crate::consensus_model::LogIndex(2),
                                    ..noop
                                },
                                payload: EntryPayload::NoOp,
                            }],
                            leader_commit: Some(noop),
                        },
                        (1, 1024),
                    )
                    .map_err(|_| VoteGateError::StorageFailure)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            executor
                .complete_read(local, context)
                .await
                .unwrap()
                .is_none()
        );
        executor.shutdown().await.unwrap();
    }

    // Validate exact identities across mixed entries and a count-boundary turn,
    // including canonical dedup/conflict outcomes and restart continuity.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn applied_batch_mixed_entries_boundary_dedup_conflict_and_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let membership = Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap();
        let mut store =
            OwnedVoteStore::bootstrap(&path, "cluster", node, membership.clone()).unwrap();
        let first = store.apply_committed_batch().unwrap();
        let genesis = first.applied.unwrap();
        assert_eq!(
            first.outcomes,
            vec![AppliedOutcome {
                identity: genesis,
                response: None
            }]
        );
        assert!(!first.more);
        let identity = |index| LogIdentity {
            term: Term(1),
            index: crate::consensus_model::LogIndex(index),
            leader: node,
        };
        let command = proposal_command();
        let mut conflict = command.clone();
        conflict.request =
            bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::CreateWorkspace {
                workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId {
                    value: uuid::Uuid::from_u128(99),
                },
                name: None,
            };
        let mut entries = vec![
            Entry {
                identity: identity(1),
                payload: EntryPayload::NoOp,
            },
            Entry {
                identity: identity(2),
                payload: EntryPayload::Membership(membership),
            },
            Entry {
                identity: identity(3),
                payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                    &command,
                )),
            },
            Entry {
                identity: identity(4),
                payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                    &command,
                )),
            },
            Entry {
                identity: identity(5),
                payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                    &conflict,
                )),
            },
        ];
        entries.extend((6..=65).map(|index| Entry {
            identity: identity(index),
            payload: EntryPayload::NoOp,
        }));
        assert!(
            store
                .append(
                    &AppendRequest {
                        term: Term(1),
                        leader: node,
                        previous: Some(genesis),
                        entries,
                        leader_commit: Some(identity(65)),
                    },
                    (128, APPLICATION_ENTRY_BUDGET)
                )
                .unwrap()
                .outcome
                .is_ok()
        );
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        let batch = executor.apply_committed_batch().await.unwrap();
        assert_eq!(batch.outcomes.len(), 64);
        assert_eq!(batch.applied, Some(identity(64)));
        assert!(batch.more);
        let mut expected_state = crate::control_state::ControlState::new("cluster");
        let expected =
            crate::control_codec::encode_control_response(&expected_state.apply(&command));
        let conflicting =
            crate::control_codec::encode_control_response(&expected_state.apply(&conflict));
        assert_ne!(expected, conflicting);
        for (index, outcome) in (1..=64).zip(&batch.outcomes) {
            assert_eq!(outcome.identity, identity(index));
            let response = match index {
                3 | 4 => Some(expected.clone()),
                5 => Some(conflicting.clone()),
                _ => None,
            };
            assert_eq!(outcome.response, response);
        }
        let state = executor.read_control_state().await.unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(executor.observe(node).await.unwrap().applied, batch.applied);
        executor.shutdown().await.unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(
            2,
            OwnedVoteStore::open(&path, "cluster").unwrap(),
        ));
        let batch = executor.apply_committed_batch().await.unwrap();
        assert_eq!(
            batch,
            AppliedBatch {
                outcomes: vec![AppliedOutcome {
                    identity: identity(65),
                    response: None
                }],
                applied: Some(identity(65)),
                more: false
            }
        );
        assert_eq!(
            executor.apply_committed().await.unwrap(),
            [] as [Vec<u8>; 0]
        );
        assert_eq!(
            executor
                .read_control_state()
                .await
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            state.encode_snapshot().unwrap()
        );
        executor.shutdown().await.unwrap();
    }

    #[test]
    fn applied_batch_byte_boundary_preserves_next_exact_identity() {
        let root = tempfile::tempdir().unwrap();
        let (mut store, _, previous) = snapshot_install_fixture(&root.path().join("authority"));
        let identity = |index| LogIdentity {
            index: crate::consensus_model::LogIndex(index),
            ..previous
        };
        let command = proposal_at_framed_size(identity(2), 600 * 1024);
        let canonical = crate::control_codec::encode_control_command(&command);
        store
            .append(
                &AppendRequest {
                    term: previous.term,
                    leader: previous.leader,
                    previous: Some(previous),
                    entries: vec![
                        Entry {
                            identity: identity(2),
                            payload: EntryPayload::Application(canonical.clone()),
                        },
                        Entry {
                            identity: identity(3),
                            payload: EntryPayload::Application(canonical),
                        },
                    ],
                    leader_commit: Some(identity(3)),
                },
                (2, 2 * APPLICATION_ENTRY_BUDGET),
            )
            .unwrap();
        let first = store.apply_committed_batch().unwrap();
        assert_eq!(first.outcomes.len(), 1);
        assert_eq!(first.outcomes[0].identity, identity(2));
        assert_eq!(first.applied, Some(identity(2)));
        assert!(first.more);
        let second = store.apply_committed_batch().unwrap();
        assert_eq!(second.outcomes.len(), 1);
        assert_eq!(second.outcomes[0].identity, identity(3));
        assert_eq!(second.outcomes[0].response, first.outcomes[0].response);
        assert_eq!(second.applied, Some(identity(3)));
        assert!(!second.more);
        let empty = store.apply_committed_batch().unwrap();
        assert_eq!(empty.outcomes, [] as [AppliedOutcome; 0]);
        assert_eq!(empty.applied, second.applied);
        assert!(!empty.more);
    }

    #[test]
    fn applied_batch_decode_failure_never_acknowledges_partial_transaction() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (mut store, _, previous) = snapshot_install_fixture(&path);
        let identity = |index| LogIdentity {
            index: crate::consensus_model::LogIndex(index),
            ..previous
        };
        let before = store
            .read_control_state()
            .unwrap()
            .encode_snapshot()
            .unwrap();
        store
            .append(
                &AppendRequest {
                    term: previous.term,
                    leader: previous.leader,
                    previous: Some(previous),
                    entries: vec![
                        Entry {
                            identity: identity(2),
                            payload: EntryPayload::NoOp,
                        },
                        Entry {
                            identity: identity(3),
                            payload: EntryPayload::Application(vec![255]),
                        },
                    ],
                    leader_commit: Some(identity(3)),
                },
                (2, 1024),
            )
            .unwrap();
        assert!(store.apply_committed_batch().is_err());
        assert!(store.failed);
        drop(store);
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.observe().unwrap().applied, Some(previous));
        assert_eq!(
            store
                .read_control_state()
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            before
        );
        assert!(store.apply_committed_batch().is_err());
        assert!(store.apply_committed().is_err());
    }

    fn proposal_command() -> bmux_cluster_plugin_api::cluster_types::ControlCommand {
        use bmux_cluster_plugin_api::cluster_types::{
            CommandId, ControlCommand, ControlCommandRequest, WorkspaceId,
        };
        ControlCommand {
            schema_version: 1,
            principal_id: "principal:test".into(),
            command_id: CommandId {
                value: uuid::Uuid::from_u128(11),
            },
            issued_at_unix_ms: 50,
            request: ControlCommandRequest::CreateWorkspace {
                workspace_id: WorkspaceId {
                    value: uuid::Uuid::from_u128(21),
                },
                name: Some("owned".into()),
            },
        }
    }

    // Keep the proposal/commit/application/restart/dedup lifecycle together.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn local_proposal_single_voter_commit_apply_reopen_dedup() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        executor.campaign(node).await.unwrap().unwrap();
        let initial = executor.initialize_leader().await.unwrap().unwrap();
        let expected = executor.observe(node).await.unwrap().hard_state;
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    expected,
                    "principal:test".into(),
                    proposal_command(),
                    |_, _| Ok(())
                )
                .await,
            Err(ProposalError::NotReady)
        ));
        assert_eq!(
            executor.commit_replicated().await.unwrap(),
            Some(initial.entries[0].identity)
        );
        executor.apply_committed().await.unwrap();
        let observation = executor.observe(node).await.unwrap();
        assert!(observation.local_leader);
        assert_eq!(
            observation.effective_membership,
            observation.committed_membership
        );
        let first = executor
            .propose_authorized(
                node,
                expected,
                "principal:test".into(),
                proposal_command(),
                |state, command| {
                    assert_eq!(state.revision, 0);
                    assert_eq!(command.principal_id, "principal:test");
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(
            executor.observe(node).await.unwrap().committed,
            observation.committed
        );
        assert_eq!(executor.commit_replicated().await.unwrap(), Some(first));
        let first_reply = executor.apply_committed().await.unwrap();
        assert_eq!(executor.read_control_state().await.unwrap().revision, 1);
        let duplicate = executor
            .propose_authorized(
                node,
                expected,
                "principal:test".into(),
                proposal_command(),
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.index.0, first.index.0 + 1);
        assert_eq!(executor.commit_replicated().await.unwrap(), Some(duplicate));
        assert_eq!(executor.apply_committed().await.unwrap(), first_reply);
        assert_eq!(executor.read_control_state().await.unwrap().revision, 1);
        executor.shutdown().await.unwrap();
        let reopened = VoteExecutor::new(DurableVoteGate::new(
            2,
            OwnedVoteStore::open(&path, "cluster").unwrap(),
        ));
        let observation = reopened.observe(node).await.unwrap();
        assert!(!observation.local_leader);
        assert_eq!(observation.hard_state, expected);
        assert_eq!(observation.last_log, Some(duplicate));
        assert_eq!(observation.committed, Some(duplicate));
        assert_eq!(observation.applied, Some(duplicate));
        let mut state = reopened.read_control_state().await.unwrap();
        let reply = state.apply(&proposal_command());
        assert_eq!(reply.control_revision, 1);
        assert_eq!(state.revision, 1);
        reopened.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn local_proposal_rejects_stale_denied_and_invalid_without_append() {
        let root = tempfile::tempdir().unwrap();
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &root.path().join("authority"),
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        let stale = executor.observe(node).await.unwrap().hard_state;
        executor.campaign(node).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        let before = executor.observe(node).await.unwrap();
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    stale,
                    "principal:test".into(),
                    proposal_command(),
                    |_, _| panic!("stale authorization ran")
                )
                .await,
            Err(ProposalError::StaleAuthority)
        ));
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "other".into(),
                    proposal_command(),
                    |_, _| Ok(())
                )
                .await,
            Err(ProposalError::Unauthorized(_))
        ));
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    proposal_command(),
                    |_, _| Err("denied".into())
                )
                .await,
            Err(ProposalError::Unauthorized(_))
        ));
        let mut invalid = proposal_command();
        invalid.schema_version = u16::MAX;
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    invalid,
                    |_, _| Ok(())
                )
                .await,
            Err(ProposalError::InvalidCommand(_))
        ));
        let mut oversized = proposal_command();
        oversized.principal_id = "x".repeat(1_048_577);
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    oversized,
                    |_, _| Ok(())
                )
                .await,
            Err(ProposalError::InvalidCommand(_))
        ));
        assert_eq!(executor.observe(node).await.unwrap(), before);
        executor.shutdown().await.unwrap();
    }

    fn proposal_at_framed_size(
        identity: LogIdentity,
        size: usize,
    ) -> bmux_cluster_plugin_api::cluster_types::ControlCommand {
        let mut command = proposal_command();
        command.request =
            bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::CompleteWorkflow {
                original_command_id: command.command_id.clone(),
                response: Vec::new(),
            };
        let overhead = crate::consensus_storage::encode_owned_entry(&Entry {
            identity,
            payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                &command,
            )),
        })
        .unwrap()
        .len();
        if let bmux_cluster_plugin_api::cluster_types::ControlCommandRequest::CompleteWorkflow {
            response,
            ..
        } = &mut command.request
        {
            response.resize(size.checked_sub(overhead).unwrap(), 0);
        }
        command
    }

    // Keep the boundary admission, committed-but-unapplied restart and recovery
    // together: an accepted command must always remain applicable after reopen.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn local_proposal_framed_boundary_is_applicable_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        executor.campaign(node).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        let before = executor.observe(node).await.unwrap();
        let identity = LogIdentity {
            term: before.hard_state.term,
            leader: node,
            index: crate::consensus_model::LogIndex(before.last_log.unwrap().index.0 + 1),
        };
        let oversized = proposal_at_framed_size(identity, APPLICATION_ENTRY_BUDGET + 1);
        assert!(
            crate::control_codec::control_command_encoded_len(&oversized, APPLICATION_ENTRY_BUDGET)
                .is_ok()
        );
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    oversized,
                    |_, _| Ok(())
                )
                .await,
            Err(ProposalError::InvalidCommand(_))
        ));
        assert_eq!(executor.observe(node).await.unwrap(), before);
        let command = proposal_at_framed_size(identity, APPLICATION_ENTRY_BUDGET);
        assert_eq!(
            crate::consensus_storage::encode_owned_entry(&Entry {
                identity,
                payload: EntryPayload::Application(crate::control_codec::encode_control_command(
                    &command
                )),
            })
            .unwrap()
            .len(),
            APPLICATION_ENTRY_BUDGET
        );
        assert_eq!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    command.clone(),
                    |_, _| Ok(())
                )
                .await
                .unwrap(),
            identity
        );
        assert_eq!(executor.commit_replicated().await.unwrap(), Some(identity));
        executor.shutdown().await.unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(
            2,
            OwnedVoteStore::open(&path, "cluster").unwrap(),
        ));
        let replies = executor.apply_committed().await.unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(
            executor.observe(node).await.unwrap().applied,
            Some(identity)
        );
        let mut state = executor.read_control_state().await.unwrap();
        let response = state.apply(&command);
        assert_eq!(
            crate::control_codec::encode_control_response(&response),
            replies[0]
        );
        executor.shutdown().await.unwrap();
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.observe().unwrap().applied, Some(identity));
    }

    // Exercise capacity rejection, continued observation and recovery after
    // compaction on one authority rather than simulating a generic I/O error.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn local_proposal_capacity_is_retryable_before_transaction() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        executor.campaign(node).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        executor
            .run(move |gate| {
                let last = gate.store.observe()?.last_log.unwrap();
                let entries = (last.index.0 + 1..MAX_RETAINED_ENTRIES)
                    .map(|index| Entry {
                        identity: LogIdentity {
                            index: crate::consensus_model::LogIndex(index),
                            ..last
                        },
                        payload: EntryPayload::NoOp,
                    })
                    .collect::<Vec<_>>();
                let through = entries.last().unwrap().identity;
                gate.store
                    .append(
                        &AppendRequest {
                            term: last.term,
                            leader: node,
                            previous: Some(last),
                            entries,
                            leader_commit: Some(through),
                        },
                        (4096, 1024),
                    )
                    .map_err(|_| VoteGateError::StorageFailure)?;
                gate.leader
                    .as_mut()
                    .unwrap()
                    .acknowledge_probe(node, Some(through));
                Ok(())
            })
            .await
            .unwrap();
        loop {
            let observation = executor.observe(node).await.unwrap();
            if observation.applied == observation.committed {
                break;
            }
            executor.apply_committed().await.unwrap();
        }
        let before = executor.observe(node).await.unwrap();
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    proposal_command(),
                    |_, _| Ok(())
                )
                .await,
            Err(ProposalError::Capacity)
        ));
        assert_eq!(executor.observe(node).await.unwrap(), before);
        executor.publish_snapshot().await.unwrap();
        executor.compact_snapshot().await.unwrap();
        let identity = executor
            .propose_authorized(
                node,
                before.hard_state,
                "principal:test".into(),
                proposal_command(),
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(identity.index.0, MAX_RETAINED_ENTRIES);
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        executor.shutdown().await.unwrap();
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.observe().unwrap().applied, Some(identity));
        assert_eq!(store.read_control_state().unwrap().revision, 1);
    }

    // Large retries used to exhaust the backend well before the periodic
    // entry-count compactor ran. Cover pressure, nonpoisoning refusal, restart,
    // explicit compaction and another acknowledged/application cycle together.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn local_proposal_byte_capacity_large_retries_restart_and_compact() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        executor.campaign(node).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        let before = executor.observe(node).await.unwrap();
        let command = proposal_at_framed_size(before.last_log.unwrap(), 900 * 1024);
        let mut accepted = 0;
        let mut original_reply = None;
        loop {
            let before = executor.observe(node).await.unwrap();
            match executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    command.clone(),
                    |_, _| Ok(()),
                )
                .await
            {
                Ok(identity) => {
                    accepted += 1;
                    assert!(
                        accepted < 70,
                        "byte admission failed to stop growth before backend exhaustion"
                    );
                    assert_eq!(executor.commit_replicated().await.unwrap(), Some(identity));
                    let replies = executor.apply_committed().await.unwrap();
                    if let Some(original) = &original_reply {
                        assert_eq!(&replies, original);
                    } else {
                        original_reply = Some(replies);
                    }
                }
                Err(ProposalError::Capacity) => {
                    assert_eq!(executor.observe(node).await.unwrap(), before);
                    break;
                }
                Err(error) => panic!("byte pressure became an ambiguous error: {error:?}"),
            }
        }
        assert!(accepted > 1);
        let full = executor.observe(node).await.unwrap();
        let canonical = executor
            .read_control_state()
            .await
            .unwrap()
            .encode_snapshot()
            .unwrap();
        executor.shutdown().await.unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(
            2,
            OwnedVoteStore::open(&path, "cluster").unwrap(),
        ));
        let reopened = executor.observe(node).await.unwrap();
        assert_eq!(reopened.applied, full.applied);
        assert_eq!(reopened.committed, full.committed);
        assert_eq!(
            executor
                .read_control_state()
                .await
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            canonical
        );
        executor.publish_snapshot().await.unwrap();
        executor.compact_snapshot().await.unwrap();
        executor.campaign(node).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        let current = executor.observe(node).await.unwrap();
        // Authorization still runs after reclaim, even for a known retry.
        assert!(matches!(
            executor
                .propose_authorized(
                    node,
                    current.hard_state,
                    "principal:test".into(),
                    command.clone(),
                    |_, _| Err("revoked".into())
                )
                .await,
            Err(ProposalError::Unauthorized(_))
        ));
        let identity = executor
            .propose_authorized(
                node,
                current.hard_state,
                "principal:test".into(),
                command,
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        executor.commit_replicated().await.unwrap();
        assert_eq!(
            executor.apply_committed().await.unwrap(),
            original_reply.unwrap()
        );
        executor.shutdown().await.unwrap();
        let mut reopened = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(reopened.observe().unwrap().applied, Some(identity));
        assert_eq!(
            reopened
                .read_control_state()
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            canonical
        );
    }

    // Keep pressure and retry in one live owner: reopening would itself drain
    // deferred pages and hide the compaction lifecycle bug this test targets.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn local_proposal_compaction_drains_deferred_pages_before_retry() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let node = NodeId::from(1);
        let store = OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        executor.campaign(node).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed().await.unwrap();
        let initial = executor.observe(node).await.unwrap();
        let command = proposal_at_framed_size(initial.last_log.unwrap(), 900 * 1024);
        let mut accepted = 0;
        loop {
            let before = executor.observe(node).await.unwrap();
            match executor
                .propose_authorized(
                    node,
                    before.hard_state,
                    "principal:test".into(),
                    command.clone(),
                    |_, _| Ok(()),
                )
                .await
            {
                Ok(identity) => {
                    accepted += 1;
                    assert!(accepted < 70);
                    assert_eq!(executor.commit_replicated().await.unwrap(), Some(identity));
                    executor.apply_committed_batch().await.unwrap();
                }
                Err(ProposalError::Capacity) => {
                    assert_eq!(executor.observe(node).await.unwrap(), before);
                    break;
                }
                Err(error) => panic!("unexpected pressure error: {error:?}"),
            }
        }
        assert!(accepted > 1);
        let allocated_before = executor
            .run(|gate| {
                let tx = gate
                    .store
                    .database
                    .begin_write()
                    .map_err(|_| VoteGateError::StorageFailure)?;
                let stats = tx.stats().map_err(|_| VoteGateError::StorageFailure)?;
                Ok(stats.allocated_pages() * stats.page_size() as u64)
            })
            .await
            .unwrap();
        let full = executor.observe(node).await.unwrap();
        let control = executor
            .read_control_state()
            .await
            .unwrap()
            .encode_snapshot()
            .unwrap();
        executor.publish_snapshot().await.unwrap();
        executor.compact_snapshot().await.unwrap();
        let allocated_after = executor
            .run(|gate| {
                let tx = gate
                    .store
                    .database
                    .begin_write()
                    .map_err(|_| VoteGateError::StorageFailure)?;
                let stats = tx.stats().map_err(|_| VoteGateError::StorageFailure)?;
                Ok(stats.allocated_pages() * stats.page_size() as u64)
            })
            .await
            .unwrap();
        assert!(
            allocated_after < allocated_before,
            "deferred allocation not reclaimed: {allocated_before} -> {allocated_after}"
        );
        assert_eq!(executor.observe(node).await.unwrap().applied, full.applied);
        let identity = executor
            .propose_authorized(
                node,
                full.hard_state,
                "principal:test".into(),
                command,
                |_, _| Ok(()),
            )
            .await
            .unwrap();
        executor.commit_replicated().await.unwrap();
        executor.apply_committed_batch().await.unwrap();
        assert_eq!(
            executor
                .read_control_state()
                .await
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            control
        );
        executor.shutdown().await.unwrap();
        let mut reopened = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(reopened.observe().unwrap().applied, Some(identity));
        assert_eq!(
            reopened
                .read_control_state()
                .unwrap()
                .encode_snapshot()
                .unwrap(),
            control
        );
    }

    fn rewrite_install_snapshot(
        bytes: &[u8],
        boundary: LogIdentity,
        membership: &crate::consensus_model::StoredMembership,
    ) -> Vec<u8> {
        let (_, control) = crate::consensus_storage::decode_owned_snapshot(bytes).unwrap();
        crate::consensus_storage::encode_owned_snapshot(
            "cluster",
            "replacement",
            boundary,
            membership,
            &control,
        )
        .unwrap()
    }

    #[test]
    fn snapshot_install_rejects_membership_rewrite_at_matching_and_missing_boundaries() {
        for compact in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("authority");
            let (mut store, bytes, boundary) = snapshot_install_fixture(&path);
            if compact {
                store.compact_snapshot().unwrap();
            }
            let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(&bytes).unwrap();
            let mut membership = descriptor.membership.unwrap();
            let node = boundary.leader;
            membership.membership =
                Membership::new(vec![vec![node]], vec![(node, "changed".into())]).unwrap();
            for boundary in [
                boundary,
                LogIdentity {
                    index: crate::consensus_model::LogIndex(4),
                    ..boundary
                },
            ] {
                let altered = rewrite_install_snapshot(&bytes, boundary, &membership);
                assert!(store.install_snapshot(store.state(), &altered).is_err());
                assert!(!store.failed);
                validate_log_recovery(&store.database.begin_read().unwrap()).unwrap();
            }
            drop(store);
            OwnedVoteStore::open(&path, "cluster").unwrap();
        }
    }

    #[test]
    fn snapshot_install_rejects_equal_term_other_leader_and_replaces_uncommitted_suffix() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (mut store, bytes, boundary) = snapshot_install_fixture(&path);
        let (descriptor, _) = crate::consensus_storage::decode_owned_snapshot(&bytes).unwrap();
        let membership = descriptor.membership.unwrap();
        let other = LogIdentity {
            leader: NodeId::from(2),
            index: crate::consensus_model::LogIndex(2),
            ..boundary
        };
        let contradiction = rewrite_install_snapshot(&bytes, other, &membership);
        assert!(
            store
                .install_snapshot(store.state(), &contradiction)
                .is_err()
        );
        assert!(!store.failed);
        let uncommitted = LogIdentity {
            index: crate::consensus_model::LogIndex(2),
            ..boundary
        };
        store
            .append(
                &AppendRequest {
                    term: boundary.term,
                    leader: boundary.leader,
                    previous: Some(boundary),
                    entries: vec![Entry {
                        identity: uncommitted,
                        payload: EntryPayload::NoOp,
                    }],
                    leader_commit: Some(boundary),
                },
                (1, 1024),
            )
            .unwrap();
        store
            .persist(HardState {
                term: Term(2),
                voted_for: None,
            })
            .unwrap();
        let replacement = LogIdentity {
            term: Term(2),
            ..uncommitted
        };
        let replacement_bytes = rewrite_install_snapshot(&bytes, replacement, &membership);
        store
            .install_snapshot(store.state(), &replacement_bytes)
            .unwrap();
        drop(store);
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        let observation = store.observe().unwrap();
        assert_eq!(observation.committed, Some(replacement));
        assert_eq!(observation.applied, Some(replacement));
        assert_eq!(observation.last_log, Some(replacement));
        assert_eq!(
            store
                .database
                .begin_read()
                .unwrap()
                .open_table(LOG)
                .unwrap()
                .len()
                .unwrap(),
            0
        );
    }

    #[test]
    fn snapshot_install_capacity_is_conservative_checked_and_nonpoisoning() {
        assert!(!snapshot_install_fits(0, 24 * 1024 * 1024, 0, 0));
        assert!(!snapshot_install_fits(u64::MAX, 1, 1, 1));
        assert!(!snapshot_install_fits(0, u64::MAX, 0, 0));
        assert!(!snapshot_install_fits(24 * 1024 * 1024, 1, 1, 1));
        assert!(!snapshot_install_fits(0, 1, 1, 8 * 1024 * 1024));
        assert!(snapshot_install_fits(1024 * 1024, 1024, 1024, 1024));
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (mut store, bytes, _) = snapshot_install_fixture(&path);
        // A large canonical control state: many individually bounded names,
        // not a malformed oversized field that the codec would reject first.
        let (descriptor, control) =
            crate::consensus_storage::decode_owned_snapshot(&bytes).unwrap();
        let mut control = crate::control_state::ControlState::decode_snapshot(&control).unwrap();
        for index in 0..6144u128 {
            let value = uuid::Uuid::from_u128(index + 100);
            control.workspaces.insert(
                value,
                bmux_cluster_plugin_api::cluster_types::WorkspaceRecord {
                    workspace_id: bmux_cluster_plugin_api::cluster_types::WorkspaceId { value },
                    name: Some("x".repeat(4096)),
                    revision: 0,
                },
            );
        }
        let large = crate::consensus_storage::encode_owned_snapshot(
            "cluster",
            "large",
            descriptor
                .last_included
                .expect("fixture snapshot is nonempty"),
            &descriptor.membership.unwrap(),
            &control.encode_snapshot().unwrap(),
        )
        .unwrap();
        assert!(large.len() > 24 * 1024 * 1024);
        assert_eq!(
            store
                .install_snapshot(store.state(), &large)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        assert!(!store.failed);
        store.install_snapshot(store.state(), &bytes).unwrap();
        drop(store);
        OwnedVoteStore::open(&path, "cluster").unwrap();
    }

    #[test]
    fn snapshot_install_capacity_counts_retained_suffix_without_poisoning() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (mut store, bytes, boundary) = snapshot_install_fixture(&path);
        let suffix = LogIdentity {
            index: crate::consensus_model::LogIndex(2),
            ..boundary
        };
        store
            .append(
                &AppendRequest {
                    term: boundary.term,
                    leader: boundary.leader,
                    previous: Some(boundary),
                    entries: vec![Entry {
                        identity: suffix,
                        payload: EntryPayload::Application(vec![0; 6 * 1024 * 1024]),
                    }],
                    leader_commit: Some(boundary),
                },
                (1, 6 * 1024 * 1024),
            )
            .unwrap();
        let before = store.observe().unwrap();
        assert_eq!(
            store
                .install_snapshot(store.state(), &bytes)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        assert!(!store.failed);
        assert_eq!(store.observe().unwrap(), before);
        drop(store);
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(store.observe().unwrap(), before);
    }

    fn snapshot_install_fixture(path: &Path) -> (OwnedVoteStore, Vec<u8>, LogIdentity) {
        let node = NodeId::from(1);
        let mut store = OwnedVoteStore::bootstrap(
            path,
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        store.apply_committed().unwrap();
        let boundary = LogIdentity {
            term: Term(1),
            index: crate::consensus_model::LogIndex(1),
            leader: node,
        };
        store
            .append(
                &AppendRequest {
                    term: Term(1),
                    leader: node,
                    previous: Some(LogIdentity {
                        term: Term(0),
                        index: crate::consensus_model::LogIndex(0),
                        leader: node,
                    }),
                    entries: vec![Entry {
                        identity: boundary,
                        payload: EntryPayload::NoOp,
                    }],
                    leader_commit: Some(boundary),
                },
                (64, 1024 * 1024),
            )
            .unwrap();
        store.apply_committed().unwrap();
        let bytes = store.publish_snapshot().unwrap();
        (store, bytes, boundary)
    }

    #[test]
    fn snapshot_install_roundtrip_preserves_vote_and_continuation() {
        let root = tempfile::tempdir().unwrap();
        let (source, bytes, boundary) = snapshot_install_fixture(&root.path().join("source"));
        let path = root.path().join("target");
        let mut target = OwnedVoteStore::create(&path, "cluster").unwrap();
        target.persist(source.state()).unwrap();
        let expected = target.state();
        target.install_snapshot(expected, &bytes).unwrap();
        target.install_snapshot(expected, &bytes).unwrap();
        drop(target);
        let mut target = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(target.state(), expected);
        let tx = target.database.begin_read().unwrap();
        validate_log_recovery(&tx).unwrap();
        let meta = tx.open_table(TABLE).unwrap();
        assert_eq!(
            meta.get("compacted_snapshot").unwrap().unwrap().value(),
            bytes
        );
        for key in ["snapshot", "commit", "applied"] {
            assert_eq!(
                decode_position(meta.get(key).unwrap().unwrap().value()).unwrap(),
                Some(boundary)
            );
        }
        drop(meta);
        drop(tx);
        let next = LogIdentity {
            index: crate::consensus_model::LogIndex(2),
            ..boundary
        };
        target
            .append(
                &AppendRequest {
                    term: expected.term,
                    leader: boundary.leader,
                    previous: Some(boundary),
                    entries: vec![Entry {
                        identity: next,
                        payload: EntryPayload::NoOp,
                    }],
                    leader_commit: Some(next),
                },
                (64, 1024 * 1024),
            )
            .unwrap();
        target.apply_committed().unwrap();
        validate_log_recovery(&target.database.begin_read().unwrap()).unwrap();
    }

    #[test]
    fn snapshot_install_preserves_committed_suffix_and_rejects_rollback() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority");
        let (mut store, bytes, boundary) = snapshot_install_fixture(&path);
        let next = LogIdentity {
            index: crate::consensus_model::LogIndex(2),
            ..boundary
        };
        store
            .append(
                &AppendRequest {
                    term: store.state().term,
                    leader: boundary.leader,
                    previous: Some(boundary),
                    entries: vec![Entry {
                        identity: next,
                        payload: EntryPayload::NoOp,
                    }],
                    leader_commit: Some(next),
                },
                (64, 1024 * 1024),
            )
            .unwrap();
        let expected = store.state();
        store.install_snapshot(expected, &bytes).unwrap();
        drop(store);
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        {
            let tx = store.database.begin_read().unwrap();
            validate_log_recovery(&tx).unwrap();
            let meta = tx.open_table(TABLE).unwrap();
            assert_eq!(
                decode_position(meta.get("commit").unwrap().unwrap().value()).unwrap(),
                Some(next)
            );
            assert_eq!(tx.open_table(LOG).unwrap().len().unwrap(), 1);
        }
        store.apply_committed().unwrap();
        assert!(store.install_snapshot(expected, &bytes).is_err());
        assert!(!store.failed);
        validate_log_recovery(&store.database.begin_read().unwrap()).unwrap();
    }

    #[test]
    fn snapshot_install_rejects_stale_corrupt_and_foreign_objects() {
        let root = tempfile::tempdir().unwrap();
        let (mut source, bytes, _) = snapshot_install_fixture(&root.path().join("source"));
        assert!(
            source
                .install_snapshot(
                    HardState {
                        term: Term(0),
                        voted_for: None
                    },
                    &bytes
                )
                .is_err()
        );
        let expected = source.state();
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        assert!(source.install_snapshot(expected, &corrupt).is_err());
        let mut unsupported = bytes.clone();
        // Durable header: eight-byte magic, then the u16 format version.
        unsupported[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        let payload = unsupported.len() - 32;
        let checksum = Sha256::digest(&unsupported[..payload]);
        unsupported[payload..].copy_from_slice(&checksum);
        assert!(source.install_snapshot(expected, &unsupported).is_err());
        source.install_snapshot(expected, &bytes).unwrap();
        let mut foreign = OwnedVoteStore::create(&root.path().join("foreign"), "other").unwrap();
        foreign.persist(expected).unwrap();
        assert!(foreign.install_snapshot(expected, &bytes).is_err());
        assert!(!foreign.failed);
        validate_log_recovery(&foreign.database.begin_read().unwrap()).unwrap();
    }

    #[test]
    fn snapshot_install_crash_child() {
        let Some(path) = std::env::var_os("BMUX_OWNED_INSTALL_PATH") else {
            return;
        };
        let bytes = std::fs::read(std::env::var_os("BMUX_OWNED_INSTALL_BYTES").unwrap()).unwrap();
        let mut store = OwnedVoteStore::open(Path::new(&path), "cluster").unwrap();
        store.install_snapshot(store.state(), &bytes).unwrap();
        panic!("snapshot install crash hook not reached");
    }

    #[test]
    fn snapshot_install_crash_recovers_one_atomic_authority() {
        for point in ["before-commit", "after-commit"] {
            let root = tempfile::tempdir().unwrap();
            let (source, bytes, boundary) = snapshot_install_fixture(&root.path().join("source"));
            let snapshot = root.path().join("snapshot");
            std::fs::write(&snapshot, &bytes).unwrap();
            let path = root.path().join("target");
            let mut target = OwnedVoteStore::create(&path, "cluster").unwrap();
            target.persist(source.state()).unwrap();
            drop(target);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owned_vote_store::tests::snapshot_install_crash_child",
                    "--nocapture",
                ])
                .env("BMUX_OWNED_INSTALL_PATH", &path)
                .env("BMUX_OWNED_INSTALL_BYTES", &snapshot)
                .env("BMUX_OWNED_INSTALL_CRASH", point)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(75));
            let target = OwnedVoteStore::open(&path, "cluster").unwrap();
            assert_eq!(target.state(), source.state());
            let tx = target.database.begin_read().unwrap();
            validate_log_recovery(&tx).unwrap();
            let meta = tx.open_table(TABLE).unwrap();
            let installed = point == "after-commit";
            for key in ["snapshot", "commit", "applied"] {
                assert_eq!(
                    decode_position(meta.get(key).unwrap().unwrap().value()).unwrap(),
                    installed.then_some(boundary)
                );
            }
            assert_eq!(meta.get("compacted_snapshot").unwrap().is_some(), installed);
        }
    }

    // Keep process-exit recovery of acknowledged application state together.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn snapshot_install_crash_replacement_preserves_acknowledged_state() {
        for point in ["before-commit", "after-commit"] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("target");
            let (mut target, _, boundary) = snapshot_install_fixture(&path);
            let command = proposal_command();
            let applied = LogIdentity {
                index: crate::consensus_model::LogIndex(2),
                ..boundary
            };
            target
                .append(
                    &AppendRequest {
                        term: boundary.term,
                        leader: boundary.leader,
                        previous: Some(boundary),
                        entries: vec![Entry {
                            identity: applied,
                            payload: EntryPayload::Application(
                                crate::control_codec::encode_control_command(&command),
                            ),
                        }],
                        leader_commit: Some(applied),
                    },
                    (1, 1_048_576),
                )
                .unwrap();
            target.apply_committed().unwrap();
            let old_control = target
                .read_control_state()
                .unwrap()
                .encode_snapshot()
                .unwrap();
            let old_snapshot = target.publish_snapshot().unwrap();
            target.compact_snapshot().unwrap();
            let (descriptor, _) =
                crate::consensus_storage::decode_owned_snapshot(&old_snapshot).unwrap();
            target
                .persist(HardState {
                    term: Term(2),
                    voted_for: None,
                })
                .unwrap();
            let expected = target.state();
            let replacement = LogIdentity {
                term: Term(2),
                index: crate::consensus_model::LogIndex(3),
                ..boundary
            };
            let bytes = rewrite_install_snapshot(
                &old_snapshot,
                replacement,
                &descriptor.membership.unwrap(),
            );
            let snapshot = root.path().join("snapshot");
            std::fs::write(&snapshot, &bytes).unwrap();
            drop(target);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "owned_vote_store::tests::snapshot_install_crash_child",
                    "--nocapture",
                ])
                .env("BMUX_OWNED_INSTALL_PATH", &path)
                .env("BMUX_OWNED_INSTALL_BYTES", &snapshot)
                .env("BMUX_OWNED_INSTALL_CRASH", point)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(75));
            let mut target = OwnedVoteStore::open(&path, "cluster").unwrap();
            assert_eq!(target.state(), expected);
            let observation = target.observe().unwrap();
            let position = if point == "before-commit" {
                applied
            } else {
                replacement
            };
            assert_eq!(observation.applied, Some(position));
            assert_eq!(observation.committed, Some(position));
            assert_eq!(
                target
                    .read_control_state()
                    .unwrap()
                    .encode_snapshot()
                    .unwrap(),
                old_control
            );
            let mut state = target.read_control_state().unwrap();
            assert_eq!(state.apply(&command).control_revision, 1);
            assert_eq!(state.revision, 1);
            validate_log_recovery(&target.database.begin_read().unwrap()).unwrap();
        }
    }

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
        // Exercise installation of the real canonical mutation/dedup state,
        // not just an empty control snapshot or reconstructed projection.
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("installed");
        let mut installed = OwnedVoteStore::create(&path, "cluster").unwrap();
        installed.persist(store.state()).unwrap();
        installed
            .install_snapshot(installed.state(), &snapshot)
            .unwrap();
        drop(installed);
        let installed = OwnedVoteStore::open(&path, "cluster").unwrap();
        let tx = installed.database.begin_read().unwrap();
        validate_log_recovery(&tx).unwrap();
        let meta = tx.open_table(TABLE).unwrap();
        assert_eq!(meta.get("control").unwrap().unwrap().value(), control);
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
                assert_eq!(replies, [] as [Vec<u8>; 0]);
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
    async fn timed_out_sealing_retains_bytes_for_revalidated_retry() {
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
        // Use the same disposable staging owner, with a private test directory.
        let directory = std::fs::File::open(root.path()).unwrap();
        let key = crate::transfer_staging::TransferKey {
            peer: *node.as_bytes(),
            incarnation: uuid::Uuid::nil(),
            sequence: 1,
        };
        let ticket = executor
            .run(move |gate| {
                let mut staging = crate::transfer_staging::TransferStaging::new(4, 4).unwrap();
                let now = std::time::Instant::now();
                staging
                    .begin(
                        &directory,
                        key,
                        [0; 32],
                        4,
                        Sha256::digest(b"data").into(),
                        now,
                    )
                    .unwrap();
                staging.chunk(key, 0, b"data", now).unwrap();
                let ticket = staging
                    .prepare_validation(key)
                    .unwrap()
                    .validate(|_, _| Ok(()))
                    .unwrap();
                gate.staging = Some((directory, staging));
                Ok(ticket)
            })
            .await
            .unwrap();
        let permit = executor.validation.clone().try_acquire_owned().unwrap();
        let guard = executor.gate.clone().lock_owned().await;
        let result = executor
            .run_admitted(true, move |gate| {
                gate.staging
                    .as_mut()
                    .unwrap()
                    .1
                    .complete_validation(key, ticket)
                    .unwrap();
                Ok(())
            })
            .await;
        assert!(matches!(result, Err(VoteGateError::Busy)));
        drop(guard);
        executor
            .run_admitted(true, move |gate| {
                let staging = &mut gate.staging.as_mut().unwrap().1;
                assert!(staging.read(key, 0, &mut [0; 4]).is_err());
                let ticket = staging
                    .prepare_validation(key)
                    .unwrap()
                    .validate(|file, total| {
                        use std::io::Read;
                        assert_eq!(total, 4);
                        let mut bytes = [0; 4];
                        file.read_exact(&mut bytes)?;
                        assert_eq!(&bytes, b"data");
                        Ok(())
                    })
                    .unwrap();
                staging.complete_validation(key, ticket).unwrap();
                let mut bytes = [0; 4];
                assert_eq!(staging.read(key, 0, &mut bytes).unwrap(), 4);
                assert_eq!(&bytes, b"data");
                Ok(())
            })
            .await
            .unwrap();
        drop(permit);
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_signals_admitted_worker_before_waiting_for_authority() {
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
        let closing = std::sync::Arc::clone(&executor.closing);
        let (entered, admitted) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let work = executor.run(move |gate| {
            entered.send(()).unwrap();
            // Bound the fixture so a regression cannot strand a blocking worker.
            released
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            assert!(closing.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(
                gate.store.status.load(std::sync::atomic::Ordering::SeqCst),
                0
            );
            Ok(())
        });
        let shutdown = async {
            admitted.await.unwrap();
            let shutdown = executor.shutdown();
            tokio::pin!(shutdown);
            // Poll shutdown while the admitted worker still owns authority.
            tokio::select! {
                biased;
                result = &mut shutdown => panic!("shutdown released live authority: {result:?}"),
                () = std::future::ready(()) => {}
            }
            assert!(executor.closing.load(std::sync::atomic::Ordering::SeqCst));
            assert!(matches!(
                executor.run(|_| Ok(())).await,
                Err(VoteGateError::Stopped)
            ));
            release.send(()).unwrap();
            shutdown.await.unwrap();
        };
        let (result, ()) = tokio::join!(work, shutdown);
        result.unwrap();
        assert!(executor.gate.lock().await.is_none());
        // Successful shutdown released the durable owner, not only admission.
        OwnedVoteStore::open(&path, "cluster")
            .unwrap()
            .close()
            .unwrap();
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
        assert_eq!(
            executor.apply_committed().await.unwrap(),
            [] as [Vec<u8>; 0]
        );
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
        assert_ne!(
            meta.get("applied_membership").unwrap().unwrap().value(),
            b""
        );
    }

    fn append_admission_request(
        local: NodeId,
        peer: NodeId,
        cluster_id: String,
    ) -> bmux_cluster_plugin_api::cluster_consensus_rpc_types::AppendRequest {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types as wire,
            cluster_consensus_transfer_types::{LogIdentity as WireIdentity, TransferProof},
            cluster_types::{PeerAuthChallenge, PeerAuthProof},
        };
        let position = WireIdentity {
            term: 0,
            index: 0,
            leader: local.as_bytes().to_vec(),
        };
        wire::AppendRequest {
            cluster_id: cluster_id.clone(),
            sender: local.as_bytes().to_vec(),
            recipient: peer.as_bytes().to_vec(),
            request_id: uuid::Uuid::new_v4(),
            term: 1,
            previous: Some(position.clone()),
            entries: Vec::new(),
            leader_commit: Some(position),
            proof: TransferProof {
                credential_serial: "serial".into(),
                signature: vec![0; 64],
                peer: PeerAuthProof {
                    challenge: PeerAuthChallenge {
                        protocol_version: 1,
                        cluster_id,
                        verifier_node_id: peer.to_string(),
                        verifier_credential_serial: "serial".into(),
                        audience_node_id: local.to_string(),
                        nonce: "nonce".into(),
                        issued_at_unix_ms: 0,
                        expires_at_unix_ms: 1,
                        signature: String::new(),
                    },
                    claimant_node_id: local.to_string(),
                    claimant_credential_serial: "serial".into(),
                    claimant_signature: String::new(),
                },
            },
        }
    }

    fn check_outbound_snapshot_probe(
        store: &mut OwnedVoteStore,
        mut leader: crate::consensus_model::LeaderProgress,
        local: NodeId,
        peer: NodeId,
        boundary: LogIdentity,
    ) {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        // Install the sender's public credential for request construction too.
        let tx = store.database.begin_write().unwrap();
        {
            let mut meta = tx.open_table(TABLE).unwrap();
            let mut state = crate::control_state::ControlState::decode_snapshot(
                meta.get("control").unwrap().unwrap().value(),
            )
            .unwrap();
            let source = &state.members[&peer.to_string()];
            let identity = crate::membership::NodeIdentity::new_for_test(1);
            assert_eq!(*identity.node_id(), local);
            let member = crate::membership::issue_test_member(
                &identity,
                source.cluster_id.parse().unwrap(),
                &identity,
                "memory://local",
                source.capabilities.clone(),
                crate::now_unix_ms(),
            );
            state.members.insert(local.to_string(), member);
            meta.insert("control", state.encode_snapshot().unwrap().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
        assert!(matches!(
            store.outbound_batch(Some(&leader), local, peer).unwrap(),
            Err(RpcError::Incompatible)
        ));
        leader
            .next
            .insert(peer, crate::consensus_model::LogIndex(1));
        let batch = store
            .outbound_batch(Some(&leader), local, peer)
            .unwrap()
            .unwrap();
        assert_eq!(batch.append.previous, Some(boundary));
        assert_eq!(batch.append.entries, [] as [Entry; 0]);
        assert_eq!(batch.append.leader_commit, Some(boundary));
        leader.term = Term(2);
        assert!(matches!(
            store.outbound_batch(Some(&leader), local, peer).unwrap(),
            Err(RpcError::StaleTerm)
        ));
    }

    #[test]
    fn outbound_append_accepts_only_exact_compacted_boundary() {
        use bmux_cluster_plugin_api::{
            cluster_consensus_rpc_types as wire,
            cluster_types::{ClusterConsensusRole, ClusterNodeCapabilities},
        };
        let root = tempfile::tempdir().unwrap();
        let local = *crate::membership::NodeIdentity::new_for_test(1).node_id();
        let remote = crate::membership::NodeIdentity::new_for_test(2);
        let peer = *remote.node_id();
        let cluster = crate::membership::ClusterId::generate();
        let mut member = crate::membership::issue_test_member(
            &remote,
            cluster,
            &remote,
            "memory://peer",
            ClusterNodeCapabilities {
                consensus_role: ClusterConsensusRole::Voter,
                worker: false,
                ingress: false,
            },
            crate::now_unix_ms(),
        );
        member.negotiated_protocol.features.extend([
            bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID.to_string(),
            bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID.to_string(),
        ]);
        let cluster_id = member.cluster_id.clone();
        let membership = Membership::new(
            vec![vec![local]],
            vec![(local, String::new()), (peer, String::new())],
        )
        .unwrap();
        let mut store = OwnedVoteStore::bootstrap(
            &root.path().join("authority.redb"),
            &cluster_id,
            local,
            membership.clone(),
        )
        .unwrap();
        let boundary = LogIdentity {
            term: Term(0),
            index: crate::consensus_model::LogIndex(0),
            leader: local,
        };
        store.apply_committed().unwrap();
        store.publish_snapshot().unwrap();
        store.compact_snapshot().unwrap();
        store
            .persist(HardState {
                term: Term(1),
                voted_for: Some(local),
            })
            .unwrap();
        let tx = store.database.begin_write().unwrap();
        {
            let mut meta = tx.open_table(TABLE).unwrap();
            let mut state = crate::control_state::ControlState::new(cluster_id.clone());
            state.members.insert(peer.to_string(), member);
            meta.insert("control", state.encode_snapshot().unwrap().as_slice())
                .unwrap();
        }
        tx.commit().unwrap();
        let leader = crate::consensus_model::LeaderProgress {
            term: Term(1),
            membership,
            matched: [(local, Some(boundary)), (peer, None)].into(),
            next: [(local, boundary.index), (peer, boundary.index)].into(),
        };
        check_outbound_snapshot_probe(&mut store, leader.clone(), local, peer, boundary);
        let mut request = append_admission_request(local, peer, cluster_id);
        assert_eq!(
            store
                .admit_replication(Some(&leader), local, &request)
                .unwrap()
                .unwrap()
                .2,
            Some(boundary)
        );
        request.previous.as_mut().unwrap().leader = peer.as_bytes().to_vec();
        assert!(matches!(
            store
                .admit_replication(Some(&leader), local, &request)
                .unwrap(),
            Err(wire::RpcError::InvalidRequest)
        ));
        request.previous.as_mut().unwrap().leader = local.as_bytes().to_vec();
        request.leader_commit.as_mut().unwrap().term = 1;
        assert!(matches!(
            store
                .admit_replication(Some(&leader), local, &request)
                .unwrap(),
            Err(wire::RpcError::InvalidRequest)
        ));
        assert!(!store.failed);
    }

    #[test]
    fn outbound_vote_requires_both_negotiated_interfaces() {
        use bmux_cluster_plugin_api::cluster_types::{
            ClusterConsensusRole, ClusterMemberState, ClusterNodeCapabilities,
        };
        let identity = crate::membership::NodeIdentity::new_for_test(1);
        let cluster = crate::membership::ClusterId::generate();
        let mut member = crate::membership::issue_test_member(
            &identity,
            cluster,
            &identity,
            "memory://node",
            ClusterNodeCapabilities {
                consensus_role: ClusterConsensusRole::Voter,
                worker: false,
                ingress: false,
            },
            crate::now_unix_ms(),
        );
        let mut state = crate::control_state::ControlState::new(member.cluster_id.clone());
        let id = *identity.node_id();
        for feature in [
            None,
            Some(bmux_cluster_plugin_api::cluster_consensus_rpc::INTERFACE_ID),
            Some(bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID),
        ] {
            if let Some(ref feature) = feature {
                member
                    .negotiated_protocol
                    .features
                    .push(feature.to_string());
            }
            state.members.insert(id.to_string(), member.clone());
            let result = owned_vote_endpoint(&state, &member.cluster_id, id);
            if feature == Some(bmux_cluster_plugin_api::cluster_consensus_transfer::INTERFACE_ID) {
                assert_eq!(result.unwrap(), "memory://node");
            } else {
                assert!(matches!(result, Err(bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError::Incompatible)));
            }
        }
        member.state = ClusterMemberState::Revoked;
        state.members.insert(id.to_string(), member.clone());
        assert!(owned_vote_endpoint(&state, &member.cluster_id, id).is_err());
    }

    #[test]
    fn application_rejects_missing_committed_entry_atomically() {
        let root = tempfile::tempdir().unwrap();
        let node = NodeId::from(1);
        let mut store = OwnedVoteStore::bootstrap(
            &root.path().join("authority.redb"),
            "cluster",
            node,
            Membership::new(vec![vec![node]], vec![(node, String::new())]).unwrap(),
        )
        .unwrap();
        let tx = store.database.begin_write().unwrap();
        tx.open_table(LOG).unwrap().remove(0).unwrap();
        tx.commit().unwrap();
        assert!(
            store
                .apply_committed()
                .unwrap_err()
                .to_string()
                .contains("missing committed")
        );
        assert!(store.failed);
        let tx = store.database.begin_read().unwrap();
        let meta = tx.open_table(TABLE).unwrap();
        assert_eq!(
            decode_position(meta.get("applied").unwrap().unwrap().value()).unwrap(),
            None
        );
        assert_eq!(
            meta.get("applied_membership").unwrap().unwrap().value(),
            b""
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
    async fn shutdown_drains_admitted_outbound_votes() {
        let root = tempfile::tempdir().unwrap();
        let store = OwnedVoteStore::create(&root.path().join("authority.redb"), "cluster").unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        let permit = executor.outbound_votes.try_acquire().unwrap();
        let shutdown = executor.shutdown();
        tokio::pin!(shutdown);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut shutdown)
                .await
                .is_err()
        );
        assert!(executor.closing.load(std::sync::atomic::Ordering::SeqCst));
        drop(permit);
        shutdown.await.unwrap();
    }

    #[test]
    fn vote_rpc_errors_preserve_retry_and_durability_meaning() {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        assert!(matches!(
            vote_rpc_error(VoteGateError::Busy),
            RpcError::CapacityUnavailable
        ));
        assert!(matches!(
            vote_rpc_error(VoteGateError::Stopped),
            RpcError::Stopped
        ));
        assert!(matches!(
            vote_rpc_error(VoteGateError::StorageFailure),
            RpcError::StorageFailure
        ));
        assert!(matches!(
            vote_rpc_error(VoteGateError::EffectExhausted),
            RpcError::ExecutionFailed
        ));
    }

    // Keep the campaign, same-term step-down and durable recovery sequence together.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn campaign_counts_distinct_voters_and_steps_down_durably() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let nodes = [NodeId::from(1), NodeId::from(2), NodeId::from(3)];
        let membership = Membership::new(
            vec![nodes.to_vec()],
            nodes.iter().map(|id| (*id, String::new())).collect(),
        )
        .unwrap();
        let founding =
            Membership::new(vec![vec![nodes[0]]], vec![(nodes[0], String::new())]).unwrap();
        let mut store = OwnedVoteStore::bootstrap(&path, "cluster", nodes[0], founding).unwrap();
        let previous = LogIdentity {
            term: Term(0),
            leader: nodes[0],
            index: crate::consensus_model::LogIndex(0),
        };
        store
            .append(
                &AppendRequest {
                    term: Term(0),
                    leader: nodes[0],
                    previous: Some(previous),
                    entries: vec![
                        crate::consensus_model::Entry {
                            identity: LogIdentity {
                                index: crate::consensus_model::LogIndex(1),
                                ..previous
                            },
                            payload: EntryPayload::Membership(
                                Membership::new(
                                    vec![vec![nodes[0]], nodes.to_vec()],
                                    nodes.iter().map(|node| (*node, String::new())).collect(),
                                )
                                .unwrap(),
                            ),
                        },
                        crate::consensus_model::Entry {
                            identity: LogIdentity {
                                index: crate::consensus_model::LogIndex(2),
                                ..previous
                            },
                            payload: EntryPayload::Membership(membership),
                        },
                    ],
                    leader_commit: Some(LogIdentity {
                        index: crate::consensus_model::LogIndex(1),
                        ..previous
                    }),
                },
                (16, 1024 * 1024),
            )
            .unwrap();
        store.close().unwrap();
        let executor = VoteExecutor::recover(path.clone(), "cluster".into(), 1)
            .await
            .unwrap();
        let request = executor.campaign(nodes[0]).await.unwrap().unwrap();
        let granted = VoteResponse {
            term: request.term,
            granted: true,
        };
        assert!(
            !executor
                .campaign_response(request.term, nodes[0], granted)
                .await
                .unwrap()
        );
        assert!(
            !executor
                .campaign_response(request.term, NodeId::from(4), granted)
                .await
                .unwrap()
        );
        assert!(
            executor
                .campaign_response(request.term, nodes[1], granted)
                .await
                .unwrap()
        );
        let newer = executor.campaign(nodes[0]).await.unwrap().unwrap();
        assert_eq!(newer.term, Term(2));
        let decision = executor
            .append(
                AppendRequest {
                    term: newer.term,
                    leader: nodes[1],
                    previous: newer.last_log,
                    entries: Vec::new(),
                    leader_commit: None,
                },
                (16, 1024 * 1024),
            )
            .await
            .unwrap();
        assert!(decision.step_down);
        assert!(
            !executor
                .campaign_response(
                    newer.term,
                    nodes[1],
                    VoteResponse {
                        term: newer.term,
                        granted: true,
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !executor
                .campaign_response(
                    request.term,
                    nodes[2],
                    VoteResponse {
                        term: Term(3),
                        granted: false
                    }
                )
                .await
                .unwrap()
        );
        assert!(
            !executor
                .campaign_response(request.term, nodes[1], granted)
                .await
                .unwrap()
        );
        executor.shutdown().await.unwrap();
        drop(executor);
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(
            store.state(),
            HardState {
                term: Term(3),
                voted_for: None
            }
        );
        store.close().unwrap();
    }

    #[tokio::test]
    async fn replicated_commit_survives_compacted_matches() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let local = NodeId::from(1);
        OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            local,
            Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap(),
        )
        .unwrap()
        .close()
        .unwrap();
        let executor = VoteExecutor::recover(path, "cluster".into(), 1)
            .await
            .unwrap();
        executor.campaign(local).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        assert!(executor.commit_replicated().await.unwrap().is_some());
        executor.apply_committed().await.unwrap();
        executor.publish_snapshot().await.unwrap();
        executor.compact_snapshot().await.unwrap();
        assert_eq!(executor.commit_replicated().await.unwrap(), None);
        executor.campaign(local).await.unwrap().unwrap();
        let next = executor.initialize_leader().await.unwrap().unwrap();
        assert_eq!(
            executor.commit_replicated().await.unwrap(),
            Some(next.entries[0].identity)
        );
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replication_completion_requires_exact_match_and_current_authority() {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::RpcError;
        let root = tempfile::tempdir().unwrap();
        let local = NodeId::from(1);
        let peer = NodeId::from(2);
        let store = OwnedVoteStore::bootstrap(
            &root.path().join("authority.redb"),
            "cluster",
            local,
            Membership::new(
                vec![vec![local]],
                vec![(local, String::new()), (peer, String::new())],
            )
            .unwrap(),
        )
        .unwrap();
        let executor = VoteExecutor::new(DurableVoteGate::new(1, store));
        assert!(matches!(
            executor.replication_status(peer).await,
            Err(RpcError::StaleTerm)
        ));
        executor.campaign(local).await.unwrap().unwrap();
        let initial = executor.initialize_leader().await.unwrap().unwrap();
        let tip = initial.entries[0].identity;
        assert!(!executor.replication_status(peer).await.unwrap().2);
        assert!(matches!(
            executor.replication_status(NodeId::from(3)).await,
            Err(RpcError::Unauthorized)
        ));
        executor
            .run(move |gate| {
                let leader = gate.leader.as_mut().unwrap();
                // Retry position is not durability evidence.
                leader.next.insert(peer, tip.index.successor().unwrap());
                Ok(())
            })
            .await
            .unwrap();
        assert!(!executor.replication_status(peer).await.unwrap().2);
        executor
            .run(move |gate| {
                gate.leader
                    .as_mut()
                    .unwrap()
                    .acknowledge_probe(peer, Some(tip));
                Ok(())
            })
            .await
            .unwrap();
        assert!(executor.replication_status(peer).await.unwrap().2);
        executor.campaign(local).await.unwrap().unwrap();
        assert!(matches!(
            executor.replication_status(peer).await,
            Err(RpcError::StaleTerm)
        ));
        executor.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn replicated_commit_is_durable_without_application() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let local = NodeId::from(1);
        OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            local,
            Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap(),
        )
        .unwrap()
        .close()
        .unwrap();
        let executor = VoteExecutor::recover(path.clone(), "cluster".into(), 1)
            .await
            .unwrap();
        executor.campaign(local).await.unwrap().unwrap();
        let initial = executor.initialize_leader().await.unwrap().unwrap();
        assert_eq!(
            executor.commit_replicated().await.unwrap(),
            Some(initial.entries[0].identity)
        );
        assert_eq!(executor.commit_replicated().await.unwrap(), None);
        executor.shutdown().await.unwrap();
        let store = OwnedVoteStore::open(&path, "cluster").unwrap();
        let tx = store.database.begin_read().unwrap();
        let meta = tx.open_table(TABLE).unwrap();
        assert_eq!(
            decode_position(meta.get("commit").unwrap().unwrap().value()).unwrap(),
            Some(initial.entries[0].identity)
        );
        assert_eq!(
            decode_position(meta.get("applied").unwrap().unwrap().value()).unwrap(),
            None
        );
        drop(meta);
        drop(tx);
        store.close().unwrap();
    }

    #[tokio::test]
    async fn leader_authority_read_failure_suppresses_success() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let local = NodeId::from(1);
        OwnedVoteStore::bootstrap(
            &path,
            "cluster",
            local,
            Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap(),
        )
        .unwrap()
        .close()
        .unwrap();
        let executor = VoteExecutor::recover(path, "cluster".into(), 1)
            .await
            .unwrap();
        executor.campaign(local).await.unwrap().unwrap();
        executor.initialize_leader().await.unwrap().unwrap();
        let result = executor
            .run(|gate| {
                let tx = gate.store.database.begin_write().unwrap();
                tx.open_table(TABLE).unwrap().remove("membership").unwrap();
                tx.commit().unwrap();
                Ok(true)
            })
            .await;
        assert_eq!(result, Err(VoteGateError::StorageFailure));
        {
            let gate = executor.gate.lock().await;
            assert!(gate.as_ref().unwrap().leader.is_none());
            assert!(gate.as_ref().unwrap().store.failed);
            drop(gate);
        }
        assert!(matches!(
            executor.campaign(local).await,
            Err(VoteGateError::Stopped)
        ));
        assert!(matches!(
            executor.shutdown().await,
            Err(VoteGateError::StorageFailure)
        ));
    }

    #[tokio::test]
    async fn campaign_persists_self_vote_and_uses_recovered_log() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let local = NodeId::from(1);
        let membership = Membership::new(vec![vec![local]], vec![(local, String::new())]).unwrap();
        OwnedVoteStore::bootstrap(&path, "cluster", local, membership)
            .unwrap()
            .close()
            .unwrap();
        let executor = VoteExecutor::recover(path.clone(), "cluster".into(), 1)
            .await
            .unwrap();
        assert!(executor.campaign(NodeId::from(2)).await.unwrap().is_none());
        let request = executor.campaign(local).await.unwrap().unwrap();
        assert_eq!(request.term, Term(1));
        assert_eq!(request.candidate, local);
        assert_eq!(request.last_log.unwrap().index.0, 0);
        let initial = executor.initialize_leader().await.unwrap().unwrap();
        assert_eq!(initial.entries[0].identity.term, Term(1));
        assert_eq!(initial.entries[0].identity.index.0, 1);
        assert!(matches!(initial.entries[0].payload, EntryPayload::NoOp));
        assert!(initial.leader_commit.is_none());
        executor
            .run(move |gate| {
                let leader = gate.leader.as_ref().unwrap();
                assert_eq!(leader.term, Term(1));
                assert_eq!(leader.matched.get(&local).unwrap().unwrap().index.0, 1);
                Ok(())
            })
            .await
            .unwrap();
        assert!(executor.initialize_leader().await.unwrap().is_none());
        executor.shutdown().await.unwrap();
        assert!(matches!(
            executor.campaign(local).await,
            Err(VoteGateError::Stopped)
        ));
        drop(executor);
        let mut recovered = OwnedVoteStore::open(&path, "cluster").unwrap();
        assert_eq!(
            recovered.election_context().unwrap().0,
            Some(initial.entries[0].identity)
        );
        let tx = recovered.database.begin_read().unwrap();
        let meta = tx.open_table(TABLE).unwrap();
        let committed = decode_position(meta.get("commit").unwrap().unwrap().value()).unwrap();
        assert_eq!(committed.unwrap().index.0, 0);
        drop(meta);
        drop(tx);
        assert_eq!(
            recovered.state(),
            HardState {
                term: Term(1),
                voted_for: Some(local)
            }
        );
        recovered.close().unwrap();
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
                    payload: EntryPayload::Membership(membership.clone()),
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
            1
        );
        let meta = transaction.open_table(TABLE).unwrap();
        assert_eq!(
            decode_position(meta.get("commit").unwrap().unwrap().value()).unwrap(),
            Some(identity(2, 1))
        );
        assert_ne!(meta.get("membership").unwrap().unwrap().value(), b"");
    }
    #[test]
    fn retained_transfer_retry_survives_reopen_without_staging() {
        use bmux_cluster_plugin_api::cluster_consensus_rpc_types::SealedEntryReference;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authority.redb");
        let leader = *crate::membership::NodeIdentity::new_for_test(1).node_id();
        let identity = LogIdentity {
            term: Term(1),
            leader,
            index: crate::consensus_model::LogIndex(0),
        };
        let entry = Entry {
            identity,
            payload: EntryPayload::NoOp,
        };
        let mut store = OwnedVoteStore::create(&path, "cluster").unwrap();
        let request = AppendRequest {
            term: Term(1),
            leader,
            previous: None,
            entries: vec![entry],
            leader_commit: None,
        };
        assert!(store.append(&request, (16, 4096)).unwrap().outcome.is_ok());
        let bytes = {
            let tx = store.database.begin_read().unwrap();
            tx.open_table(LOG)
                .unwrap()
                .get(0)
                .unwrap()
                .unwrap()
                .value()
                .to_vec()
        };
        store.close().unwrap();
        let mut store = OwnedVoteStore::open(&path, "cluster").unwrap();
        let reference = SealedEntryReference {
            sender_incarnation: uuid::Uuid::from_u128(1),
            sequence: 1,
            identity: bmux_cluster_plugin_api::cluster_consensus_transfer_types::LogIdentity {
                term: 1,
                leader: leader.as_bytes().to_vec(),
                index: 0,
            },
            total_bytes: bytes.len() as u64,
            checksum: Sha256::digest(&bytes).to_vec(),
        };
        assert_eq!(
            store.retained_transfer_entry(&reference, 4096).unwrap(),
            Some(bytes)
        );
        let mut conflicting = reference.clone();
        conflicting.checksum[0] ^= 1;
        assert!(
            store
                .retained_transfer_entry(&conflicting, 4096)
                .unwrap()
                .is_none()
        );
        conflicting = reference.clone();
        conflicting.identity.term = 2;
        assert!(
            store
                .retained_transfer_entry(&conflicting, 4096)
                .unwrap()
                .is_none()
        );
        conflicting = reference.clone();
        conflicting.total_bytes += 1;
        assert!(
            store
                .retained_transfer_entry(&conflicting, 4096)
                .unwrap()
                .is_none()
        );
        assert!(store.retained_transfer_entry(&reference, 0).is_err());
        assert!(!store.failed);
        assert!(store.append(&request, (16, 4096)).unwrap().outcome.is_ok());
        store.close().unwrap();
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
