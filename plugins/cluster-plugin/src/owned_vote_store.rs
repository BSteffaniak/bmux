//! The owned consensus durable authority (ADR-0015).
//!
//! One exclusively owned redb store holds the vote, log, memberships, applied
//! control state and snapshots. Blocking calls belong on the driver's
//! serialized storage worker.
use crate::consensus_model::{
    AppendDecision, AppendOutcome, AppendReconciliation, AppendRequest, Entry, EntryPayload,
    HardState, LogIdentity, Membership, Term,
};
use crate::membership::NodeId;
use redb::ReadableTableMetadata;
use redb::{
    Database, Durability, ReadableDatabase, ReadableTable, StorageBackend, TableDefinition,
};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::path::PathBuf;
use switchy_fs::sync::{File, OpenOptions};

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
        file.try_lock()?;
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
        file.try_lock()?;
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

/// Bounds concurrent recovery scratch copies among the stores that share it.
///
/// Never queues startup behind slow recovery; the caller may retry explicit Busy.
/// Each owner (a plugin instance, a simulated run) holds its own, so independent
/// owners never contend through process-wide state.
#[derive(Debug, Clone, Default)]
pub struct PreflightAdmission(std::sync::Arc<std::sync::Mutex<()>>);

impl PreflightAdmission {
    fn admit(&self) -> io::Result<std::sync::MutexGuard<'_, ()>> {
        self.0.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "owned preflight busy")
            }
            std::sync::TryLockError::Poisoned(_) => {
                io::Error::other("owned preflight stopped after panic")
            }
        })
    }
}

// Tests share one admission so they can observe its backpressure; production
// owners (plugin instances, simulated runs) always pass their own.
#[cfg(test)]
static TEST_PREFLIGHT: std::sync::LazyLock<PreflightAdmission> =
    std::sync::LazyLock::new(PreflightAdmission::default);

#[cfg(test)]
fn admit_preflight() -> io::Result<std::sync::MutexGuard<'static, ()>> {
    TEST_PREFLIGHT.admit()
}

fn preflight_payload(
    backend: &OwnedBackend,
    cluster: &str,
    admission: &PreflightAdmission,
) -> io::Result<()> {
    let admission = admission.admit()?;
    let scratch = switchy_fs::sync::tempfile()?;
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

#[cfg(test)]
fn validate_staging_directory(directory: &Path) -> io::Result<()> {
    crate::transfer_staging::open_directory(directory).map(|_| ())
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

/// An exclusively created initialization inode, removed unless published.
struct InitializationFile(Option<PathBuf>);

impl InitializationFile {
    /// Names are deterministic (first free slot) so simulated runs replay.
    fn create(parent: &Path) -> io::Result<(Self, File)> {
        for slot in 0..1024u32 {
            let path = parent.join(format!(".owned-vote-init-{slot}"));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => return Ok((Self(Some(path)), file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "owned initialization slots exhausted",
        ))
    }

    /// Publish without replacing an existing authority.
    fn publish(mut self, path: &Path) -> io::Result<()> {
        let temporary = self.0.take().ok_or_else(|| invalid("already published"))?;
        switchy_fs::sync::rename_noreplace(&temporary, path).inspect_err(|_| {
            self.0 = Some(temporary.clone());
        })
    }
}

impl Drop for InitializationFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = switchy_fs::sync::remove_file(path);
        }
    }
}

/// Sealed snapshot bytes staged from a leader, found by sender and boundary.
pub type StagedSnapshot = Box<dyn FnMut(NodeId, LogIdentity) -> io::Result<Vec<u8>> + Send>;

/// The owned store as the consensus driver's durable authority.
pub struct OwnedStorage {
    store: OwnedVoteStore,
    staged: StagedSnapshot,
    members: Option<crate::consensus_wire::MemberView>,
}

impl OwnedStorage {
    #[must_use]
    pub fn new(store: OwnedVoteStore, staged: StagedSnapshot) -> Self {
        Self {
            store,
            staged,
            members: None,
        }
    }

    /// Keep `members` current with applied member credentials for the wire adapter.
    /// # Errors
    /// Reports failure to read the applied state.
    pub fn with_members(mut self, members: crate::consensus_wire::MemberView) -> io::Result<Self> {
        self.members = Some(members);
        self.refresh_members()?;
        Ok(self)
    }

    /// The applied control state with the membership it was applied under.
    /// # Errors
    /// Reports a stopped store or unreadable applied authority.
    pub fn applied(
        &mut self,
    ) -> io::Result<(
        crate::control_state::ControlState,
        Option<crate::consensus_model::StoredMembership>,
    )> {
        self.store.read_applied()
    }

    fn refresh_members(&mut self) -> io::Result<()> {
        let Some(view) = &self.members else {
            return Ok(());
        };
        let state = self.store.read_control_state()?;
        let cluster = state
            .members
            .values()
            .next()
            .map(|member| member.cluster_id.clone())
            .unwrap_or_default();
        view.update(cluster, state.members.into_values().collect());
        Ok(())
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
        self.store.install_snapshot(expected, &bytes)?;
        self.refresh_members()
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
                self.refresh_members()?;
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

    fn snapshot(&mut self) -> io::Result<Option<crate::consensus_driver::SnapshotObject>> {
        // Compaction and remote installs only ever use the published snapshot,
        // so the latest one always covers every compacted entry.
        let bytes = match self.store.published_snapshot()? {
            Some(bytes) => bytes,
            None => self.store.publish_snapshot()?,
        };
        let (descriptor, _) =
            crate::consensus_storage::decode_owned_snapshot(&bytes).map_err(io::Error::other)?;
        let (Some(last_included), Some(membership)) =
            (descriptor.last_included, descriptor.membership)
        else {
            return Ok(None);
        };
        Ok(Some(crate::consensus_driver::SnapshotObject {
            bytes,
            last_included,
            membership,
        }))
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
        // A no-replace rename publishes the committed inode without replacing any
        // existing authority; concurrent initializers have exactly one winner.
        let (temporary, file) = InitializationFile::create(parent)?;
        let backend = OwnedBackend::initialize(file, cluster)?;
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
        temporary.publish(path)?;
        #[cfg(test)]
        initialization_crash("after-publish");
        switchy_fs::sync::sync_directory(parent)?;
        Ok(Self {
            database,
            status,
            state,
            failed: false,
        })
    }

    /// [`Self::open_with`] under the tests' shared admission.
    /// # Errors
    /// As [`Self::open_with`].
    #[cfg(test)]
    pub(crate) fn open(path: &Path, cluster: &str) -> io::Result<Self> {
        Self::open_with(path, cluster, &TEST_PREFLIGHT)
    }

    /// Recover a committed owned vote with an exact cluster binding, bounding
    /// the recovery copy with the caller's own admission.
    /// # Errors
    /// Missing metadata, unknown formats, wrong clusters and corrupt votes
    /// fail; `WouldBlock` when that admission is busy. redb holds exclusive
    /// database ownership for this handle's lifetime.
    pub fn open_with(
        path: &Path,
        cluster: &str,
        admission: &PreflightAdmission,
    ) -> io::Result<Self> {
        // Validate identification while holding the same inode lock that redb
        // retains through recovery. Unsupported files never reach writable redb.
        let backend = OwnedBackend::open(path, cluster)?;
        preflight_payload(&backend, cluster, admission)?;
        // A previous initializer may have published this inode but failed before
        // directory sync. Complete that durability boundary before exposing it.
        backend.operate(|file| file.sync_all())?;
        switchy_fs::sync::sync_directory(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?;
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
    /// Replay a retained snapshot install receipt: `Some(true)` for the same
    /// signed reference, `Some(false)` for conflicting reuse, `None` if unknown.
    /// # Errors
    /// Reports a stopped store or unreadable receipts.
    pub fn install_replay(
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

    fn published_snapshot(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let tx = self.database.begin_read().map_err(io::Error::other)?;
        let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
        let bytes = meta
            .get("published_snapshot")
            .map_err(io::Error::other)?
            .map(|bytes| bytes.value().to_vec());
        self.failed = false;
        Ok(bytes)
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

    /// The applied control state and the membership it was applied under, read
    /// in one transaction.
    fn read_applied(
        &mut self,
    ) -> io::Result<(
        crate::control_state::ControlState,
        Option<crate::consensus_model::StoredMembership>,
    )> {
        if self.failed || self.status.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(io::Error::other("owned authority stopped"));
        }
        self.failed = true;
        let tx = self.database.begin_read().map_err(io::Error::other)?;
        let meta = tx.open_table(TABLE).map_err(io::Error::other)?;
        let get = |key| -> io::Result<Vec<u8>> {
            Ok(meta
                .get(key)
                .map_err(io::Error::other)?
                .ok_or_else(|| invalid("missing applied authority"))?
                .value()
                .to_vec())
        };
        let state = crate::control_state::ControlState::decode_snapshot(&get("control")?)
            .map_err(io::Error::other)?;
        let membership_bytes = get("applied_membership")?;
        let membership = if membership_bytes.is_empty() {
            None
        } else {
            let entry = crate::consensus_storage::decode_owned_entry(&membership_bytes)
                .map_err(io::Error::other)?;
            let EntryPayload::Membership(membership) = entry.payload else {
                return Err(invalid("invalid applied membership"));
            };
            Some(crate::consensus_model::StoredMembership {
                introduced_at: Some(entry.identity),
                membership,
            })
        };
        self.failed = false;
        Ok((state, membership))
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

    use super::*;

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
    fn crash_child() {
        let Some(path) = std::env::var_os("BMUX_OWNED_VOTE_CRASH_PATH") else {
            return;
        };
        let path = Path::new(&path);
        let mut store = OwnedVoteStore::open(path, "cluster-a").unwrap();
        // The vote is acknowledged only once persist returns.
        store
            .persist(HardState {
                term: Term(2),
                voted_for: Some(NodeId::from(1)),
            })
            .unwrap();
        if std::env::var_os("BMUX_OWNED_VOTE_ABORT").is_some() {
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
