#![allow(clippy::result_large_err)] // OpenRaft's required StorageError carries detailed defensive context.

//! Crash-safe `OpenRaft` storage-v2 log and hard-state persistence.
//!
//! The durable format is intentionally byte-oriented: BMUX owns every key and value
//! encoding instead of coupling consensus compatibility to Rust type layout.

use crate::control_codec::{decode_control_command, encode_control_response};
use crate::control_state::ControlState;
use openraft::storage::{LogFlushed, RaftLogStorage, RaftSnapshotBuilder, RaftStateMachine};
use openraft::{
    AnyError, BasicNode, CommittedLeaderId, Entry, EntryPayload, LeaderId, LogId, LogState,
    Membership, RaftLogReader, Snapshot, SnapshotMeta, StorageError, StorageIOError,
    StoredMembership, Vote,
};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::ops::{Bound, RangeBounds};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

const STORAGE_BLOCKING_CONCURRENCY: usize = 4;
#[cfg(test)]
const TEST_CRASH_EXIT_CODE: i32 = 86;

#[cfg(test)]
fn test_crash_point(point: &str) {
    if std::env::var("BMUX_CONSENSUS_CRASH_POINT").as_deref() == Ok(point) {
        std::process::exit(TEST_CRASH_EXIT_CODE);
    }
}

#[cfg(not(test))]
const fn test_crash_point(_point: &str) {}

fn storage_blocking_permits() -> &'static Arc<tokio::sync::Semaphore> {
    static PERMITS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    PERMITS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(STORAGE_BLOCKING_CONCURRENCY)))
}

#[allow(clippy::result_large_err)]
async fn run_storage_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, StorageError<u64>> + Send + 'static,
) -> Result<T, StorageError<u64>> {
    let permit = storage_blocking_permits()
        .clone()
        .acquire_owned()
        .await
        .map_err(storage_write_error)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(storage_write_error)?
}

const STORAGE_FORMAT_VERSION: u16 = 1;
const CONSENSUS_DB_FILE: &str = "raft.redb";
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const HARD_STATE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("hard_state");
const LOG_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("log");
const STATE_MACHINE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("state_machine");
const META_FORMAT_VERSION: &str = "storage_format_version";
const META_CLUSTER_ID: &str = "cluster_id";
const META_ACTIVE_SNAPSHOT_ID: &str = "active_snapshot_id";
const META_ACTIVE_SNAPSHOT_CHECKSUM: &str = "active_snapshot_checksum";
const HARD_STATE_VOTE: &str = "vote";
const HARD_STATE_LAST_PURGED: &str = "last_purged_log_id";
const STATE_MACHINE_LAST_APPLIED: &str = "last_applied_log_id";
const STATE_MACHINE_MEMBERSHIP: &str = "last_membership";
const STATE_MACHINE_CONTROL: &str = "control_state";
const SNAPSHOT_FORMAT_VERSION: u16 = 1;
const MAX_SNAPSHOT_FILE_BYTES: usize = 300 * 1024 * 1024;
const SNAPSHOT_INSTALL_MANIFEST: &str = "snapshot-install.manifest";

struct SnapshotEnvelope {
    cluster_id: String,
    snapshot_id: String,
    last_log_id: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, BasicNode>,
    control_bytes: Vec<u8>,
}

struct PreparedStateMachine {
    control: Vec<u8>,
    membership: Vec<u8>,
    last_applied: Option<Vec<u8>>,
}

struct SnapshotInstallManifest {
    snapshot_id: String,
    checksum: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlRequest(pub Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlReply(pub Vec<u8>);

openraft::declare_raft_types!(pub ControlRaftConfig:
    D = ControlRequest,
    R = ControlReply,
    NodeId = u64,
    Node = BasicNode,
    Entry = Entry<ControlRaftConfig>,
    SnapshotData = Cursor<Vec<u8>>,
);

#[derive(Debug)]
pub enum ConsensusStorageError {
    InvalidClusterId,
    ClusterIdMismatch { expected: String, actual: String },
    UnsupportedFormat(u16),
    CorruptRecord(&'static str),
    Io(std::io::Error),
    Database(redb::Error),
}

impl std::fmt::Display for ConsensusStorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidClusterId => {
                formatter.write_str("cluster ID is not safe for a state path")
            }
            Self::ClusterIdMismatch { expected, actual } => write!(
                formatter,
                "consensus database belongs to cluster {actual}, not expected cluster {expected}"
            ),
            Self::UnsupportedFormat(version) => {
                write!(formatter, "unsupported consensus storage format {version}")
            }
            Self::CorruptRecord(record) => write!(formatter, "corrupt consensus {record} record"),
            Self::Io(error) => write!(formatter, "consensus storage IO failed: {error}"),
            Self::Database(error) => write!(formatter, "consensus database failed: {error}"),
        }
    }
}

impl std::error::Error for ConsensusStorageError {}

impl From<std::io::Error> for ConsensusStorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<redb::Error> for ConsensusStorageError {
    fn from(error: redb::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Clone)]
pub struct ConsensusLogStore {
    database: Arc<Database>,
    database_path: PathBuf,
    cluster_id: String,
}

#[derive(Clone)]
pub struct ConsensusStateMachine {
    database: Arc<Database>,
    cluster_id: String,
    snapshot_dir: PathBuf,
    control_state: ControlState,
    last_applied: Option<LogId<u64>>,
    last_membership: StoredMembership<u64, BasicNode>,
}

impl ConsensusLogStore {
    /// Opens or initializes the database for exactly one cluster identity.
    ///
    /// # Errors
    ///
    /// Fails closed for unsafe identities, incompatible formats, identity mismatch,
    /// corrupt metadata, or database/IO errors.
    pub fn open(state_dir: &Path, cluster_id: &str) -> Result<Self, ConsensusStorageError> {
        validate_cluster_id(cluster_id)?;
        let directory = state_dir
            .join("plugins")
            .join("bmux.cluster")
            .join("consensus")
            .join(cluster_id);
        fs::create_dir_all(directory.join("snapshots"))?;
        recover_snapshot_publication(&directory)?;
        let database_path = directory.join(CONSENSUS_DB_FILE);
        let database = Arc::new(Database::create(&database_path).map_err(redb::Error::from)?);
        initialize_metadata(&database, cluster_id)?;
        reconcile_snapshot_manifest(&directory, &database)?;
        Ok(Self {
            database,
            database_path,
            cluster_id: cluster_id.to_owned(),
        })
    }

    /// Creates a fresh isolated store for `OpenRaft`'s storage-v2 conformance suite.
    #[cfg(test)]
    fn open_for_suite(
        state_dir: &Path,
    ) -> Result<(Self, ConsensusStateMachine), ConsensusStorageError> {
        let store = Self::open(state_dir, "suite-cluster")?;
        let state_machine = store.state_machine()?;
        Ok((store, state_machine))
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Opens the state-machine half over the same transactional database.
    ///
    /// # Errors
    ///
    /// Fails closed if any persisted state-machine record is corrupt or belongs
    /// to another cluster.
    pub fn state_machine(&self) -> Result<ConsensusStateMachine, ConsensusStorageError> {
        let snapshot_dir = self.database_path.with_file_name("snapshots");
        ConsensusStateMachine::open(self.database.clone(), &self.cluster_id, snapshot_dir)
    }

    fn read_vote(&self) -> Result<Option<Vote<u64>>, ConsensusStorageError> {
        read_optional_bytes(
            &self.database,
            HARD_STATE_TABLE,
            HARD_STATE_VOTE,
            decode_vote,
        )
    }

    fn read_last_purged(&self) -> Result<Option<LogId<u64>>, ConsensusStorageError> {
        read_optional_bytes(
            &self.database,
            HARD_STATE_TABLE,
            HARD_STATE_LAST_PURGED,
            decode_log_id,
        )
    }

    fn read_entries(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> Result<Vec<Entry<ControlRaftConfig>>, StorageError<u64>> {
        let read = self.database.begin_read().map_err(storage_read_error)?;
        let table = read.open_table(LOG_TABLE).map_err(storage_read_error)?;
        let mut entries = Vec::new();
        for row in table.range((start, end)).map_err(storage_read_error)? {
            let (_, value) = row.map_err(storage_read_error)?;
            entries.push(decode_entry(value.value()).map_err(storage_read_error)?);
        }
        Ok(entries)
    }

    fn log_state(&self) -> Result<LogState<ControlRaftConfig>, StorageError<u64>> {
        let last_purged_log_id = self.read_last_purged().map_err(storage_read_error)?;
        let read = self.database.begin_read().map_err(storage_read_error)?;
        let table = read.open_table(LOG_TABLE).map_err(storage_read_error)?;
        let last_present = table
            .last()
            .map_err(storage_read_error)?
            .map(|(_, value)| decode_entry(value.value()))
            .transpose()
            .map_err(storage_read_error)?
            .map(|entry: Entry<ControlRaftConfig>| entry.log_id);
        Ok(LogState {
            last_log_id: last_present.or(last_purged_log_id),
            last_purged_log_id,
        })
    }
}

impl RaftLogReader<ControlRaftConfig> for ConsensusLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<ControlRaftConfig>>, StorageError<u64>> {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();
        let store = self.clone();
        run_storage_blocking(move || store.read_entries(start, end)).await
    }
}

impl RaftLogStorage<ControlRaftConfig> for ConsensusLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<ControlRaftConfig>, StorageError<u64>> {
        let store = self.clone();
        run_storage_blocking(move || store.log_state()).await
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let database = self.database.clone();
        let encoded = encode_vote(vote);
        run_storage_blocking(move || {
            immediate_write(&database, |transaction| {
                transaction
                    .open_table(HARD_STATE_TABLE)?
                    .insert(HARD_STATE_VOTE, encoded.as_slice())?;
                Ok(())
            })
            .map_err(storage_write_error)
        })
        .await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        let store = self.clone();
        run_storage_blocking(move || store.read_vote().map_err(storage_read_error)).await
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<ControlRaftConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<ControlRaftConfig>> + Send,
        I::IntoIter: Send,
    {
        let encoded = entries
            .into_iter()
            .map(|entry| Ok((entry.log_id.index, encode_entry(&entry)?)))
            .collect::<Result<Vec<_>, ConsensusStorageError>>()
            .map_err(storage_write_error)?;
        let database = self.database.clone();
        let result = run_storage_blocking(move || {
            immediate_write(&database, |transaction| {
                let mut table = transaction.open_table(LOG_TABLE)?;
                for (index, entry) in &encoded {
                    table.insert(*index, entry.as_slice())?;
                }
                Ok(())
            })
            .map_err(storage_write_error)
        })
        .await;
        match result {
            Ok(()) => {
                test_crash_point("log-append-after-commit");
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                callback.log_io_completed(Err(std::io::Error::other(message)));
                Err(error)
            }
        }
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let database = self.database.clone();
        run_storage_blocking(move || {
            immediate_write(&database, |transaction| {
                let mut table = transaction.open_table(LOG_TABLE)?;
                let keys = table
                    .range(log_id.index..)?
                    .map(|row| row.map(|(key, _)| key.value()))
                    .collect::<Result<Vec<_>, _>>()?;
                for key in keys {
                    table.remove(key)?;
                }
                Ok(())
            })
            .map_err(storage_write_error)
        })
        .await
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let database = self.database.clone();
        let encoded = encode_log_id(&log_id);
        run_storage_blocking(move || {
            immediate_write(&database, |transaction| {
                {
                    let mut table = transaction.open_table(LOG_TABLE)?;
                    let keys = table
                        .range(..=log_id.index)?
                        .map(|row| row.map(|(key, _)| key.value()))
                        .collect::<Result<Vec<_>, _>>()?;
                    for key in keys {
                        table.remove(key)?;
                    }
                }
                transaction
                    .open_table(HARD_STATE_TABLE)?
                    .insert(HARD_STATE_LAST_PURGED, encoded.as_slice())?;
                Ok(())
            })
            .map_err(storage_write_error)
        })
        .await
    }
}

impl ConsensusStateMachine {
    fn open(
        database: Arc<Database>,
        cluster_id: &str,
        snapshot_dir: PathBuf,
    ) -> Result<Self, ConsensusStorageError> {
        let control_state = read_optional_bytes(
            &database,
            STATE_MACHINE_TABLE,
            STATE_MACHINE_CONTROL,
            |bytes| {
                ControlState::decode_snapshot(bytes)
                    .map_err(|_| ConsensusStorageError::CorruptRecord("control state"))
            },
        )?
        .unwrap_or_else(|| ControlState::new(cluster_id));
        if control_state.cluster_id != cluster_id {
            return Err(ConsensusStorageError::ClusterIdMismatch {
                expected: cluster_id.to_owned(),
                actual: control_state.cluster_id,
            });
        }
        let last_applied = read_optional_bytes(
            &database,
            STATE_MACHINE_TABLE,
            STATE_MACHINE_LAST_APPLIED,
            decode_log_id,
        )?;
        let last_membership = read_optional_bytes(
            &database,
            STATE_MACHINE_TABLE,
            STATE_MACHINE_MEMBERSHIP,
            decode_stored_membership,
        )?
        .unwrap_or_default();
        let state_machine = Self {
            database,
            cluster_id: cluster_id.to_owned(),
            snapshot_dir,
            control_state,
            last_applied,
            last_membership,
        };
        state_machine.validate_active_snapshot()?;
        Ok(state_machine)
    }

    fn prepared_state_machine(&self) -> Result<PreparedStateMachine, ConsensusStorageError> {
        Ok(PreparedStateMachine {
            control: self
                .control_state
                .encode_snapshot()
                .map_err(|_| ConsensusStorageError::CorruptRecord("control state encoding"))?,
            membership: encode_stored_membership(&self.last_membership)?,
            last_applied: self.last_applied.as_ref().map(encode_log_id),
        })
    }

    fn persist_prepared(
        database: &Database,
        prepared: &PreparedStateMachine,
    ) -> Result<(), ConsensusStorageError> {
        immediate_write(database, |transaction| {
            let mut table = transaction.open_table(STATE_MACHINE_TABLE)?;
            table.insert(STATE_MACHINE_CONTROL, prepared.control.as_slice())?;
            table.insert(STATE_MACHINE_MEMBERSHIP, prepared.membership.as_slice())?;
            match prepared.last_applied.as_deref() {
                Some(log_id) => {
                    table.insert(STATE_MACHINE_LAST_APPLIED, log_id)?;
                }
                None => {
                    table.remove(STATE_MACHINE_LAST_APPLIED)?;
                }
            }
            Ok(())
        })
    }

    #[must_use]
    pub const fn control_state(&self) -> &ControlState {
        &self.control_state
    }

    fn validate_active_snapshot(&self) -> Result<(), ConsensusStorageError> {
        let Some((snapshot_id, expected_checksum)) = read_active_snapshot_meta(&self.database)?
        else {
            return Ok(());
        };
        let path = snapshot_path(&self.snapshot_dir, &snapshot_id)?;
        let bytes = read_snapshot_file(&path)?;
        let actual_checksum = snapshot_checksum(&bytes);
        if actual_checksum != expected_checksum {
            return Err(ConsensusStorageError::CorruptRecord("snapshot checksum"));
        }
        let envelope = decode_snapshot_envelope(&bytes)?;
        if envelope.snapshot_id != snapshot_id || envelope.cluster_id != self.cluster_id {
            return Err(ConsensusStorageError::CorruptRecord("snapshot metadata"));
        }
        Ok(())
    }

    fn publish_snapshot(
        database: &Database,
        snapshot_dir: &Path,
        meta: &SnapshotMeta<u64, BasicNode>,
        cluster_id: &str,
        control_bytes: &[u8],
    ) -> Result<Vec<u8>, ConsensusStorageError> {
        validate_snapshot_id(&meta.snapshot_id)?;
        let envelope = encode_snapshot_envelope(cluster_id, meta, control_bytes)?;
        fs::create_dir_all(snapshot_dir)?;
        let final_path = snapshot_path(snapshot_dir, &meta.snapshot_id)?;
        let tmp_path = snapshot_dir.join(format!("{}.tmp", meta.snapshot_id));
        let directory = snapshot_dir
            .parent()
            .ok_or(ConsensusStorageError::CorruptRecord("snapshot directory"))?;
        let manifest = SnapshotInstallManifest {
            snapshot_id: meta.snapshot_id.clone(),
            checksum: snapshot_checksum(&envelope),
        };
        write_snapshot_manifest(directory, &manifest)?;
        test_crash_point("snapshot-after-manifest-sync");
        write_snapshot_file(&tmp_path, &envelope)?;
        test_crash_point("snapshot-after-tmp-sync");
        fs::rename(&tmp_path, &final_path)?;
        sync_directory(snapshot_dir)?;
        test_crash_point("snapshot-after-rename-sync");
        let checksum = snapshot_checksum(&envelope);
        immediate_write(database, |transaction| {
            let mut table = transaction.open_table(META_TABLE)?;
            table.insert(META_ACTIVE_SNAPSHOT_ID, meta.snapshot_id.as_bytes())?;
            table.insert(META_ACTIVE_SNAPSHOT_CHECKSUM, checksum.as_slice())?;
            Ok(())
        })?;
        test_crash_point("snapshot-after-meta-commit");
        remove_snapshot_manifest(directory)?;
        test_crash_point("snapshot-after-manifest-remove");
        remove_inactive_snapshots(snapshot_dir, &final_path)?;
        Ok(envelope)
    }
}

impl RaftStateMachine<ControlRaftConfig> for ConsensusStateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError<u64>> {
        Ok((self.last_applied, self.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<ControlReply>, StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<ControlRaftConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut next = self.clone();
        let mut replies = Vec::new();
        for entry in entries {
            next.last_applied = Some(entry.log_id);
            match entry.payload {
                EntryPayload::Blank => replies.push(ControlReply(Vec::new())),
                EntryPayload::Membership(membership) => {
                    next.last_membership = StoredMembership::new(next.last_applied, membership);
                    replies.push(ControlReply(Vec::new()));
                }
                EntryPayload::Normal(request) => {
                    let command =
                        decode_control_command(&request.0).map_err(storage_write_error)?;
                    let response = next.control_state.apply(&command);
                    replies.push(ControlReply(encode_control_response(&response)));
                }
            }
        }
        let prepared = next.prepared_state_machine().map_err(storage_write_error)?;
        let database = next.database.clone();
        run_storage_blocking(move || {
            Self::persist_prepared(&database, &prepared).map_err(storage_write_error)
        })
        .await?;
        test_crash_point("state-machine-after-commit");
        *self = next;
        Ok(replies)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        let received = snapshot.into_inner();
        let control_bytes = match decode_snapshot_envelope(&received) {
            Ok(envelope) => {
                if envelope.snapshot_id != meta.snapshot_id
                    || envelope.last_log_id != meta.last_log_id
                    || envelope.last_membership != meta.last_membership
                {
                    return Err(storage_write_error(ConsensusStorageError::CorruptRecord(
                        "snapshot metadata mismatch",
                    )));
                }
                envelope.control_bytes
            }
            Err(_) => received,
        };
        let control_state =
            ControlState::decode_snapshot(&control_bytes).map_err(storage_write_error)?;
        if control_state.cluster_id != self.cluster_id {
            return Err(storage_write_error(
                ConsensusStorageError::ClusterIdMismatch {
                    expected: self.cluster_id.clone(),
                    actual: control_state.cluster_id,
                },
            ));
        }
        let mut next = self.clone();
        next.control_state = control_state;
        next.last_applied = meta.last_log_id;
        next.last_membership = meta.last_membership.clone();
        let prepared = next.prepared_state_machine().map_err(storage_write_error)?;
        let database = next.database.clone();
        run_storage_blocking(move || {
            Self::persist_prepared(&database, &prepared).map_err(storage_write_error)
        })
        .await?;
        let snapshot_dir = next.snapshot_dir.clone();
        let cluster_id = next.cluster_id.clone();
        let database = next.database.clone();
        let meta = meta.clone();
        run_storage_blocking(move || {
            Self::publish_snapshot(&database, &snapshot_dir, &meta, &cluster_id, &control_bytes)
                .map(|_| ())
                .map_err(storage_write_error)
        })
        .await?;
        *self = next;
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<ControlRaftConfig>>, StorageError<u64>> {
        let database = self.database.clone();
        let snapshot_dir = self.snapshot_dir.clone();
        run_storage_blocking(move || {
            let Some((snapshot_id, expected_checksum)) =
                read_active_snapshot_meta(&database).map_err(storage_read_error)?
            else {
                return Ok(None);
            };
            let path = snapshot_path(&snapshot_dir, &snapshot_id).map_err(storage_read_error)?;
            let bytes = read_snapshot_file(&path).map_err(storage_read_error)?;
            if snapshot_checksum(&bytes) != expected_checksum {
                return Err(storage_read_error(ConsensusStorageError::CorruptRecord(
                    "snapshot checksum",
                )));
            }
            let envelope = decode_snapshot_envelope(&bytes).map_err(storage_read_error)?;
            Ok(Some(Snapshot {
                meta: SnapshotMeta {
                    last_log_id: envelope.last_log_id,
                    last_membership: envelope.last_membership,
                    snapshot_id: envelope.snapshot_id,
                },
                snapshot: Box::new(Cursor::new(bytes)),
            }))
        })
        .await
    }
}

impl RaftSnapshotBuilder<ControlRaftConfig> for ConsensusStateMachine {
    async fn build_snapshot(&mut self) -> Result<Snapshot<ControlRaftConfig>, StorageError<u64>> {
        let bytes = self
            .control_state
            .encode_snapshot()
            .map_err(storage_read_error)?;
        let last = self
            .last_applied
            .map_or_else(|| "none".to_string(), |log_id| log_id.to_string());
        let snapshot_id = format!("{last}-{}", self.control_state.revision);
        let meta = SnapshotMeta {
            last_log_id: self.last_applied,
            last_membership: self.last_membership.clone(),
            snapshot_id,
        };
        let snapshot_dir = self.snapshot_dir.clone();
        let cluster_id = self.cluster_id.clone();
        let database = self.database.clone();
        let publish_meta = meta.clone();
        let bytes = run_storage_blocking(move || {
            Self::publish_snapshot(&database, &snapshot_dir, &publish_meta, &cluster_id, &bytes)
                .map_err(storage_write_error)
        })
        .await?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(bytes)),
        })
    }
}

fn validate_snapshot_id(snapshot_id: &str) -> Result<(), ConsensusStorageError> {
    if snapshot_id.is_empty()
        || snapshot_id == "."
        || snapshot_id == ".."
        || snapshot_id.contains('/')
        || snapshot_id.contains('\\')
        || snapshot_id.chars().any(char::is_control)
    {
        return Err(ConsensusStorageError::CorruptRecord("snapshot ID"));
    }
    Ok(())
}

fn snapshot_path(directory: &Path, snapshot_id: &str) -> Result<PathBuf, ConsensusStorageError> {
    validate_snapshot_id(snapshot_id)?;
    Ok(directory.join(format!("{snapshot_id}.snapshot")))
}

fn snapshot_checksum(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn encode_snapshot_envelope(
    cluster_id: &str,
    meta: &SnapshotMeta<u64, BasicNode>,
    control_bytes: &[u8],
) -> Result<Vec<u8>, ConsensusStorageError> {
    if control_bytes.len() > MAX_SNAPSHOT_FILE_BYTES {
        return Err(ConsensusStorageError::CorruptRecord("snapshot size"));
    }
    let mut writer = DurableWriter::new(*b"BMSNP001");
    writer.u16(SNAPSHOT_FORMAT_VERSION);
    writer.bytes(cluster_id.as_bytes())?;
    writer.bytes(meta.snapshot_id.as_bytes())?;
    writer.boolean(meta.last_log_id.is_some());
    if let Some(log_id) = &meta.last_log_id {
        encode_log_id_fields(&mut writer, log_id);
    }
    let membership = encode_stored_membership(&meta.last_membership)?;
    writer.bytes(&membership)?;
    writer.bytes(control_bytes)?;
    let without_checksum = writer.finish();
    let checksum = snapshot_checksum(&without_checksum);
    let mut result = without_checksum;
    result.extend_from_slice(&checksum);
    Ok(result)
}

fn decode_snapshot_envelope(bytes: &[u8]) -> Result<SnapshotEnvelope, ConsensusStorageError> {
    if bytes.len() > MAX_SNAPSHOT_FILE_BYTES {
        return Err(ConsensusStorageError::CorruptRecord("snapshot size"));
    }
    let payload_len = bytes
        .len()
        .checked_sub(32)
        .ok_or(ConsensusStorageError::CorruptRecord("snapshot checksum"))?;
    let (payload, checksum) = bytes.split_at(payload_len);
    if snapshot_checksum(payload).as_slice() != checksum {
        return Err(ConsensusStorageError::CorruptRecord("snapshot checksum"));
    }
    let mut reader = DurableReader::new(payload, *b"BMSNP001", "snapshot envelope")?;
    let version = reader.u16()?;
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(ConsensusStorageError::UnsupportedFormat(version));
    }
    let cluster_id = String::from_utf8(reader.bytes()?)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot cluster ID"))?;
    let snapshot_id = String::from_utf8(reader.bytes()?)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot ID"))?;
    validate_snapshot_id(&snapshot_id)?;
    let last_log_id = if reader.boolean()? {
        Some(decode_log_id_fields(&mut reader)?)
    } else {
        None
    };
    let last_membership = decode_stored_membership(&reader.bytes()?)?;
    let control_bytes = reader.bytes()?;
    reader.finish()?;
    let control = ControlState::decode_snapshot(&control_bytes)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot control state"))?;
    if control.cluster_id != cluster_id {
        return Err(ConsensusStorageError::CorruptRecord(
            "snapshot cluster mismatch",
        ));
    }
    Ok(SnapshotEnvelope {
        cluster_id,
        snapshot_id,
        last_log_id,
        last_membership,
        control_bytes,
    })
}

fn encode_snapshot_install_manifest(
    manifest: &SnapshotInstallManifest,
) -> Result<Vec<u8>, ConsensusStorageError> {
    validate_snapshot_id(&manifest.snapshot_id)?;
    let mut writer = DurableWriter::new(*b"BMSIM001");
    writer.u16(1);
    writer.bytes(manifest.snapshot_id.as_bytes())?;
    writer.bytes(&manifest.checksum)?;
    Ok(writer.finish())
}

fn decode_snapshot_install_manifest(
    bytes: &[u8],
) -> Result<SnapshotInstallManifest, ConsensusStorageError> {
    let mut reader = DurableReader::new(bytes, *b"BMSIM001", "snapshot manifest")?;
    if reader.u16()? != 1 {
        return Err(ConsensusStorageError::CorruptRecord(
            "snapshot manifest version",
        ));
    }
    let snapshot_id = String::from_utf8(reader.bytes()?)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot manifest ID"))?;
    validate_snapshot_id(&snapshot_id)?;
    let checksum = reader
        .bytes()?
        .try_into()
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot manifest checksum"))?;
    reader.finish()?;
    Ok(SnapshotInstallManifest {
        snapshot_id,
        checksum,
    })
}

fn write_snapshot_manifest(
    directory: &Path,
    manifest: &SnapshotInstallManifest,
) -> Result<(), ConsensusStorageError> {
    let final_path = directory.join(SNAPSHOT_INSTALL_MANIFEST);
    let tmp_path = directory.join(format!("{SNAPSHOT_INSTALL_MANIFEST}.tmp"));
    if tmp_path.exists() {
        fs::remove_file(&tmp_path)?;
    }
    write_snapshot_file(&tmp_path, &encode_snapshot_install_manifest(manifest)?)?;
    fs::rename(tmp_path, &final_path)?;
    sync_directory(directory)
}

fn remove_snapshot_manifest(directory: &Path) -> Result<(), ConsensusStorageError> {
    let path = directory.join(SNAPSHOT_INSTALL_MANIFEST);
    if path.exists() {
        fs::remove_file(path)?;
        sync_directory(directory)?;
    }
    Ok(())
}

fn recover_snapshot_publication(directory: &Path) -> Result<(), ConsensusStorageError> {
    let manifest_path = directory.join(SNAPSHOT_INSTALL_MANIFEST);
    let temporary_manifest = directory.join(format!("{SNAPSHOT_INSTALL_MANIFEST}.tmp"));
    if temporary_manifest.exists() {
        fs::remove_file(temporary_manifest)?;
        sync_directory(directory)?;
    }
    if !manifest_path.exists() {
        return Ok(());
    }
    let manifest = decode_snapshot_install_manifest(&read_snapshot_file(&manifest_path)?)?;
    let snapshot_dir = directory.join("snapshots");
    let final_path = snapshot_path(&snapshot_dir, &manifest.snapshot_id)?;
    let tmp_path = snapshot_dir.join(format!("{}.tmp", manifest.snapshot_id));
    if final_path.exists() {
        let bytes = read_snapshot_file(&final_path)?;
        if snapshot_checksum(&bytes) != manifest.checksum {
            return Err(ConsensusStorageError::CorruptRecord(
                "snapshot manifest checksum",
            ));
        }
    } else if tmp_path.exists() {
        let bytes = read_snapshot_file(&tmp_path)?;
        if snapshot_checksum(&bytes) != manifest.checksum {
            return Err(ConsensusStorageError::CorruptRecord(
                "snapshot manifest checksum",
            ));
        }
        fs::rename(&tmp_path, &final_path)?;
        sync_directory(&snapshot_dir)?;
    }
    Ok(())
}

fn write_snapshot_file(path: &Path, bytes: &[u8]) -> Result<(), ConsensusStorageError> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_snapshot_file(path: &Path) -> Result<Vec<u8>, ConsensusStorageError> {
    let file = File::open(path)?;
    let size = usize::try_from(file.metadata()?.len())
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot size"))?;
    if size > MAX_SNAPSHOT_FILE_BYTES {
        return Err(ConsensusStorageError::CorruptRecord("snapshot size"));
    }
    let mut bytes = Vec::with_capacity(size);
    file.take(u64::try_from(MAX_SNAPSHOT_FILE_BYTES).expect("snapshot bound fits u64") + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SNAPSHOT_FILE_BYTES {
        return Err(ConsensusStorageError::CorruptRecord("snapshot size"));
    }
    Ok(bytes)
}

fn sync_directory(path: &Path) -> Result<(), ConsensusStorageError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn read_active_snapshot_meta(
    database: &Database,
) -> Result<Option<(String, [u8; 32])>, ConsensusStorageError> {
    let read = database.begin_read().map_err(redb::Error::from)?;
    let table = read.open_table(META_TABLE).map_err(redb::Error::from)?;
    let id = table
        .get(META_ACTIVE_SNAPSHOT_ID)
        .map_err(redb::Error::from)?
        .map(|value| {
            String::from_utf8(value.value().to_vec())
                .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot ID"))
        })
        .transpose()?;
    let checksum = table
        .get(META_ACTIVE_SNAPSHOT_CHECKSUM)
        .map_err(redb::Error::from)?
        .map(|value| {
            value
                .value()
                .try_into()
                .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot checksum"))
        })
        .transpose()?;
    match (id, checksum) {
        (None, None) => Ok(None),
        (Some(id), Some(checksum)) => Ok(Some((id, checksum))),
        _ => Err(ConsensusStorageError::CorruptRecord("snapshot metadata")),
    }
}

fn remove_inactive_snapshots(directory: &Path, active: &Path) -> Result<(), ConsensusStorageError> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path != active
            && path
                .extension()
                .is_some_and(|extension| extension == "snapshot")
        {
            fs::remove_file(path)?;
        }
    }
    sync_directory(directory)
}

fn reconcile_snapshot_manifest(
    directory: &Path,
    database: &Database,
) -> Result<(), ConsensusStorageError> {
    let manifest_path = directory.join(SNAPSHOT_INSTALL_MANIFEST);
    if !manifest_path.exists() {
        return Ok(());
    }
    let manifest = decode_snapshot_install_manifest(&read_snapshot_file(&manifest_path)?)?;
    match read_active_snapshot_meta(database)? {
        Some((snapshot_id, checksum))
            if snapshot_id == manifest.snapshot_id && checksum == manifest.checksum =>
        {
            remove_snapshot_manifest(directory)
        }
        None => Ok(()),
        Some(_) => Err(ConsensusStorageError::CorruptRecord(
            "snapshot manifest metadata mismatch",
        )),
    }
}

fn validate_cluster_id(cluster_id: &str) -> Result<(), ConsensusStorageError> {
    if cluster_id.is_empty()
        || cluster_id == "."
        || cluster_id == ".."
        || cluster_id.contains('/')
        || cluster_id.contains('\\')
        || cluster_id.chars().any(char::is_control)
    {
        return Err(ConsensusStorageError::InvalidClusterId);
    }
    Ok(())
}

fn initialize_metadata(database: &Database, cluster_id: &str) -> Result<(), ConsensusStorageError> {
    immediate_write(database, |transaction| {
        drop(transaction.open_table(META_TABLE)?);
        drop(transaction.open_table(HARD_STATE_TABLE)?);
        drop(transaction.open_table(LOG_TABLE)?);
        drop(transaction.open_table(STATE_MACHINE_TABLE)?);
        Ok(())
    })?;
    let read = database.begin_read().map_err(redb::Error::from)?;
    let table = read.open_table(META_TABLE).map_err(redb::Error::from)?;
    let stored_version = table
        .get(META_FORMAT_VERSION)
        .map_err(redb::Error::from)?
        .map(|value| decode_u16(value.value()))
        .transpose()?;
    let stored_cluster = table
        .get(META_CLUSTER_ID)
        .map_err(redb::Error::from)?
        .map(|value| {
            std::str::from_utf8(value.value())
                .map(str::to_owned)
                .map_err(|_| ConsensusStorageError::CorruptRecord("cluster ID"))
        })
        .transpose()?;
    drop(table);
    drop(read);

    match stored_version {
        Some(version) if version != STORAGE_FORMAT_VERSION => {
            return Err(ConsensusStorageError::UnsupportedFormat(version));
        }
        Some(_) => {}
        None if stored_cluster.is_some() => {
            return Err(ConsensusStorageError::CorruptRecord("storage version"));
        }
        None => {
            immediate_write(database, |transaction| {
                let mut table = transaction.open_table(META_TABLE)?;
                table.insert(
                    META_FORMAT_VERSION,
                    STORAGE_FORMAT_VERSION.to_be_bytes().as_slice(),
                )?;
                table.insert(META_CLUSTER_ID, cluster_id.as_bytes())?;
                Ok(())
            })?;
        }
    }
    if let Some(actual) = stored_cluster
        && actual != cluster_id
    {
        return Err(ConsensusStorageError::ClusterIdMismatch {
            expected: cluster_id.to_owned(),
            actual,
        });
    }
    Ok(())
}

fn immediate_write(
    database: &Database,
    operation: impl FnOnce(&redb::WriteTransaction) -> Result<(), redb::Error>,
) -> Result<(), ConsensusStorageError> {
    let mut transaction = database.begin_write().map_err(redb::Error::from)?;
    transaction
        .set_durability(Durability::Immediate)
        .map_err(redb::Error::from)?;
    operation(&transaction)?;
    transaction.commit().map_err(redb::Error::from)?;
    Ok(())
}

fn read_optional_bytes<T>(
    database: &Database,
    table: TableDefinition<&str, &[u8]>,
    key: &str,
    decode: impl FnOnce(&[u8]) -> Result<T, ConsensusStorageError>,
) -> Result<Option<T>, ConsensusStorageError> {
    let read = database.begin_read().map_err(redb::Error::from)?;
    let table = read.open_table(table).map_err(redb::Error::from)?;
    table
        .get(key)
        .map_err(redb::Error::from)?
        .map(|value| decode(value.value()))
        .transpose()
}

fn encode_vote(vote: &Vote<u64>) -> Vec<u8> {
    let mut writer = DurableWriter::new(*b"BMVOT001");
    writer.u64(vote.leader_id.term);
    writer.optional_u64(vote.leader_id.voted_for());
    writer.boolean(vote.committed);
    writer.finish()
}

fn decode_vote(bytes: &[u8]) -> Result<Vote<u64>, ConsensusStorageError> {
    let mut reader = DurableReader::new(bytes, *b"BMVOT001", "vote")?;
    let term = reader.u64()?;
    let voted_for = reader.optional_u64()?;
    let committed = reader.boolean()?;
    reader.finish()?;
    Ok(Vote {
        leader_id: LeaderId::new(
            term,
            voted_for.ok_or(ConsensusStorageError::CorruptRecord("vote"))?,
        ),
        committed,
    })
}

fn encode_log_id(log_id: &LogId<u64>) -> Vec<u8> {
    let mut writer = DurableWriter::new(*b"BMLOGID1");
    encode_log_id_fields(&mut writer, log_id);
    writer.finish()
}

fn decode_log_id(bytes: &[u8]) -> Result<LogId<u64>, ConsensusStorageError> {
    let mut reader = DurableReader::new(bytes, *b"BMLOGID1", "log ID")?;
    let log_id = decode_log_id_fields(&mut reader)?;
    reader.finish()?;
    Ok(log_id)
}

fn encode_stored_membership(
    membership: &StoredMembership<u64, BasicNode>,
) -> Result<Vec<u8>, ConsensusStorageError> {
    let mut writer = DurableWriter::new(*b"BMMEM001");
    writer.boolean(membership.log_id().is_some());
    if let Some(log_id) = membership.log_id() {
        encode_log_id_fields(&mut writer, log_id);
    }
    encode_membership_fields(&mut writer, membership.membership())?;
    Ok(writer.finish())
}

fn decode_stored_membership(
    bytes: &[u8],
) -> Result<StoredMembership<u64, BasicNode>, ConsensusStorageError> {
    let mut reader = DurableReader::new(bytes, *b"BMMEM001", "membership")?;
    let log_id = if reader.boolean()? {
        Some(decode_log_id_fields(&mut reader)?)
    } else {
        None
    };
    let membership = decode_membership_fields(&mut reader)?;
    reader.finish()?;
    Ok(StoredMembership::new(log_id, membership))
}

fn encode_membership_fields(
    writer: &mut DurableWriter,
    membership: &Membership<u64, BasicNode>,
) -> Result<(), ConsensusStorageError> {
    writer.count(membership.get_joint_config().len())?;
    for config in membership.get_joint_config() {
        writer.count(config.len())?;
        for node_id in config {
            writer.u64(*node_id);
        }
    }
    let nodes = membership.nodes().collect::<Vec<_>>();
    writer.count(nodes.len())?;
    for (node_id, node) in nodes {
        writer.u64(*node_id);
        writer.bytes(node.addr.as_bytes())?;
    }
    Ok(())
}

fn decode_membership_fields(
    reader: &mut DurableReader<'_>,
) -> Result<Membership<u64, BasicNode>, ConsensusStorageError> {
    let configs = (0..reader.count()?)
        .map(|_| {
            (0..reader.count()?)
                .map(|_| reader.u64())
                .collect::<Result<std::collections::BTreeSet<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let nodes = (0..reader.count()?)
        .map(|_| {
            let node_id = reader.u64()?;
            let address = String::from_utf8(reader.bytes()?)
                .map_err(|_| ConsensusStorageError::CorruptRecord("membership"))?;
            Ok::<_, ConsensusStorageError>((node_id, BasicNode::new(address)))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    Ok(Membership::new(configs, nodes))
}

fn encode_entry(entry: &Entry<ControlRaftConfig>) -> Result<Vec<u8>, ConsensusStorageError> {
    let mut writer = DurableWriter::new(*b"BMENT001");
    encode_log_id_fields(&mut writer, &entry.log_id);
    match &entry.payload {
        EntryPayload::Blank => writer.u8(0),
        EntryPayload::Normal(request) => {
            writer.u8(1);
            writer.bytes(&request.0)?;
        }
        EntryPayload::Membership(membership) => {
            writer.u8(2);
            encode_membership_fields(&mut writer, membership)?;
        }
    }
    Ok(writer.finish())
}

fn decode_entry(bytes: &[u8]) -> Result<Entry<ControlRaftConfig>, ConsensusStorageError> {
    let mut reader = DurableReader::new(bytes, *b"BMENT001", "log entry")?;
    let log_id = decode_log_id_fields(&mut reader)?;
    let payload = match reader.u8()? {
        0 => EntryPayload::Blank,
        1 => EntryPayload::Normal(ControlRequest(reader.bytes()?)),
        2 => EntryPayload::Membership(decode_membership_fields(&mut reader)?),
        _ => return Err(ConsensusStorageError::CorruptRecord("log entry")),
    };
    reader.finish()?;
    Ok(Entry { log_id, payload })
}

fn encode_log_id_fields(writer: &mut DurableWriter, log_id: &LogId<u64>) {
    writer.u64(log_id.leader_id.term);
    writer.u64(log_id.leader_id.node_id);
    writer.u64(log_id.index);
}

fn decode_log_id_fields(
    reader: &mut DurableReader<'_>,
) -> Result<LogId<u64>, ConsensusStorageError> {
    Ok(LogId::new(
        CommittedLeaderId::new(reader.u64()?, reader.u64()?),
        reader.u64()?,
    ))
}

struct DurableWriter {
    bytes: Vec<u8>,
}

impl DurableWriter {
    fn new(magic: [u8; 8]) -> Self {
        Self {
            bytes: magic.to_vec(),
        }
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn boolean(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn optional_u64(&mut self, value: Option<u64>) {
        self.boolean(value.is_some());
        if let Some(value) = value {
            self.u64(value);
        }
    }

    fn count(&mut self, value: usize) -> Result<(), ConsensusStorageError> {
        let value =
            u32::try_from(value).map_err(|_| ConsensusStorageError::CorruptRecord("encoding"))?;
        self.u32(value);
        Ok(())
    }

    fn bytes(&mut self, value: &[u8]) -> Result<(), ConsensusStorageError> {
        self.count(value.len())?;
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct DurableReader<'a> {
    bytes: &'a [u8],
    position: usize,
    record: &'static str,
}

impl<'a> DurableReader<'a> {
    fn new(
        bytes: &'a [u8],
        magic: [u8; 8],
        record: &'static str,
    ) -> Result<Self, ConsensusStorageError> {
        if !bytes.starts_with(&magic) {
            return Err(ConsensusStorageError::CorruptRecord(record));
        }
        Ok(Self {
            bytes,
            position: magic.len(),
            record,
        })
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], ConsensusStorageError> {
        let end = self
            .position
            .checked_add(count)
            .ok_or(ConsensusStorageError::CorruptRecord(self.record))?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or(ConsensusStorageError::CorruptRecord(self.record))?;
        self.position = end;
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, ConsensusStorageError> {
        Ok(self.take(1)?[0])
    }

    fn boolean(&mut self) -> Result<bool, ConsensusStorageError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ConsensusStorageError::CorruptRecord(self.record)),
        }
    }

    fn u16(&mut self) -> Result<u16, ConsensusStorageError> {
        let bytes = self
            .take(2)?
            .try_into()
            .map_err(|_| ConsensusStorageError::CorruptRecord(self.record))?;
        Ok(u16::from_be_bytes(bytes))
    }

    fn u32(&mut self) -> Result<u32, ConsensusStorageError> {
        let bytes = self
            .take(4)?
            .try_into()
            .map_err(|_| ConsensusStorageError::CorruptRecord(self.record))?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, ConsensusStorageError> {
        let bytes = self
            .take(8)?
            .try_into()
            .map_err(|_| ConsensusStorageError::CorruptRecord(self.record))?;
        Ok(u64::from_be_bytes(bytes))
    }

    fn optional_u64(&mut self) -> Result<Option<u64>, ConsensusStorageError> {
        if self.boolean()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }

    fn count(&mut self) -> Result<usize, ConsensusStorageError> {
        usize::try_from(self.u32()?).map_err(|_| ConsensusStorageError::CorruptRecord(self.record))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ConsensusStorageError> {
        let count = self.count()?;
        Ok(self.take(count)?.to_vec())
    }

    const fn finish(self) -> Result<(), ConsensusStorageError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(ConsensusStorageError::CorruptRecord(self.record))
        }
    }
}

fn decode_u16(bytes: &[u8]) -> Result<u16, ConsensusStorageError> {
    let bytes: [u8; 2] = bytes
        .try_into()
        .map_err(|_| ConsensusStorageError::CorruptRecord("storage version"))?;
    Ok(u16::from_be_bytes(bytes))
}

fn storage_read_error(error: impl std::error::Error + Send + Sync + 'static) -> StorageError<u64> {
    StorageIOError::read(AnyError::new(&error)).into()
}

fn storage_write_error(error: impl std::error::Error + Send + Sync + 'static) -> StorageError<u64> {
    StorageIOError::write(AnyError::new(&error)).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::storage::RaftLogStorageExt;
    use openraft::{CommittedLeaderId, EntryPayload};
    use std::process::Command;
    use tempfile::TempDir;

    const CRASH_HELPER_TEST: &str = "consensus_storage::tests::consensus_storage_crash_helper";

    fn run_crash_helper(root: &Path, mode: &str, point: &str) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(CRASH_HELPER_TEST)
            .arg("--nocapture")
            .env("BMUX_CONSENSUS_CRASH_HELPER_ROOT", root)
            .env("BMUX_CONSENSUS_CRASH_HELPER_MODE", mode)
            .env("BMUX_CONSENSUS_CRASH_POINT", point)
            .status()
            .unwrap()
    }

    #[test]
    fn consensus_storage_crash_helper() {
        let Ok(root) = std::env::var("BMUX_CONSENSUS_CRASH_HELPER_ROOT") else {
            return;
        };
        let mode = std::env::var("BMUX_CONSENSUS_CRASH_HELPER_MODE").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut store = ConsensusLogStore::open(Path::new(&root), "cluster-a").unwrap();
            match mode.as_str() {
                "append" => {
                    store
                        .blocking_append([entry(7, 1, b"committed")])
                        .await
                        .unwrap();
                }
                "state-machine" => {
                    let mut state_machine = store.state_machine().unwrap();
                    state_machine
                        .apply([Entry {
                            log_id: log_id(7, 1),
                            payload: EntryPayload::Blank,
                        }])
                        .await
                        .unwrap();
                }
                "snapshot" => {
                    let mut state_machine = store.state_machine().unwrap();
                    state_machine.build_snapshot().await.unwrap();
                }
                other => panic!("unknown crash-helper mode {other}"),
            }
        });
    }

    struct SuiteBuilder;

    impl
        openraft::testing::StoreBuilder<
            ControlRaftConfig,
            ConsensusLogStore,
            ConsensusStateMachine,
            TempDir,
        > for SuiteBuilder
    {
        async fn build(
            &self,
        ) -> Result<(TempDir, ConsensusLogStore, ConsensusStateMachine), StorageError<u64>>
        {
            let root = TempDir::new().map_err(storage_write_error)?;
            let (store, state_machine) =
                ConsensusLogStore::open_for_suite(root.path()).map_err(storage_write_error)?;
            Ok((root, store, state_machine))
        }
    }

    #[test]
    fn openraft_storage_v2_conformance_suite() {
        openraft::testing::Suite::<
            ControlRaftConfig,
            ConsensusLogStore,
            ConsensusStateMachine,
            SuiteBuilder,
            TempDir,
        >::test_all(SuiteBuilder)
        .unwrap();
    }

    fn log_id(term: u64, index: u64) -> LogId<u64> {
        LogId::new(CommittedLeaderId::new(term, 1), index)
    }

    fn entry(term: u64, index: u64, value: &[u8]) -> Entry<ControlRaftConfig> {
        Entry {
            log_id: log_id(term, index),
            payload: EntryPayload::Normal(ControlRequest(value.to_vec())),
        }
    }

    #[test]
    fn durable_commit_survives_process_exit_before_acknowledgement() {
        let root = TempDir::new().unwrap();
        let status = run_crash_helper(root.path(), "append", "log-append-after-commit");
        assert_eq!(status.code(), Some(TEST_CRASH_EXIT_CODE));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut reopened = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
            assert_eq!(
                reopened.try_get_log_entries(..).await.unwrap(),
                vec![entry(7, 1, b"committed")]
            );
        });

        let state_root = TempDir::new().unwrap();
        let status = run_crash_helper(
            state_root.path(),
            "state-machine",
            "state-machine-after-commit",
        );
        assert_eq!(status.code(), Some(TEST_CRASH_EXIT_CODE));
        let reopened = ConsensusLogStore::open(state_root.path(), "cluster-a").unwrap();
        let mut state_machine = reopened.state_machine().unwrap();
        let (last_applied, _) = runtime.block_on(state_machine.applied_state()).unwrap();
        assert_eq!(last_applied, Some(log_id(7, 1)));
    }

    #[test]
    fn interrupted_snapshot_publication_recovers_only_committed_state() {
        for point in [
            "snapshot-after-manifest-sync",
            "snapshot-after-tmp-sync",
            "snapshot-after-rename-sync",
        ] {
            let root = TempDir::new().unwrap();
            let status = run_crash_helper(root.path(), "snapshot", point);
            assert_eq!(status.code(), Some(TEST_CRASH_EXIT_CODE));
            let reopened = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let mut state_machine = reopened.state_machine().unwrap();
            assert!(
                runtime
                    .block_on(state_machine.get_current_snapshot())
                    .unwrap()
                    .is_none()
            );
            if point != "snapshot-after-manifest-sync" {
                let directory = root.path().join("plugins/bmux.cluster/consensus/cluster-a");
                let manifest = decode_snapshot_install_manifest(
                    &fs::read(directory.join(SNAPSHOT_INSTALL_MANIFEST)).unwrap(),
                )
                .unwrap();
                assert!(
                    snapshot_path(&directory.join("snapshots"), &manifest.snapshot_id)
                        .unwrap()
                        .is_file()
                );
            }
        }

        let root = TempDir::new().unwrap();
        let status = run_crash_helper(root.path(), "snapshot", "snapshot-after-meta-commit");
        assert_eq!(status.code(), Some(TEST_CRASH_EXIT_CODE));
        let reopened = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut state_machine = reopened.state_machine().unwrap();
        assert!(
            runtime
                .block_on(state_machine.get_current_snapshot())
                .unwrap()
                .is_some()
        );
        let directory = root.path().join("plugins/bmux.cluster/consensus/cluster-a");
        assert!(!directory.join(SNAPSHOT_INSTALL_MANIFEST).exists());

        let finalized_root = TempDir::new().unwrap();
        let status = run_crash_helper(
            finalized_root.path(),
            "snapshot",
            "snapshot-after-manifest-remove",
        );
        assert_eq!(status.code(), Some(TEST_CRASH_EXIT_CODE));
        let reopened = ConsensusLogStore::open(finalized_root.path(), "cluster-a").unwrap();
        let mut state_machine = reopened.state_machine().unwrap();
        assert!(
            runtime
                .block_on(state_machine.get_current_snapshot())
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn vote_log_truncate_and_purge_survive_restart() {
        let root = TempDir::new().unwrap();
        let mut store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        assert!(
            store
                .database_path()
                .ends_with("plugins/bmux.cluster/consensus/cluster-a/raft.redb")
        );

        let vote = Vote::new_committed(7, 1);
        store.save_vote(&vote).await.unwrap();
        store
            .blocking_append([
                entry(7, 1, b"one"),
                entry(7, 2, b"two"),
                entry(7, 3, b"three"),
            ])
            .await
            .unwrap();
        store.truncate(log_id(7, 3)).await.unwrap();
        store.purge(log_id(7, 1)).await.unwrap();
        drop(store);

        let mut reopened = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        assert_eq!(reopened.read_vote().unwrap(), Some(vote));
        assert_eq!(
            reopened.try_get_log_entries(..).await.unwrap(),
            vec![entry(7, 2, b"two")]
        );
        assert_eq!(
            reopened.get_log_state().await.unwrap(),
            LogState {
                last_purged_log_id: Some(log_id(7, 1)),
                last_log_id: Some(log_id(7, 2)),
            }
        );
    }

    #[test]
    fn rejects_unsafe_or_mismatched_cluster_identity() {
        let root = TempDir::new().unwrap();
        assert!(matches!(
            ConsensusLogStore::open(root.path(), "../escape"),
            Err(ConsensusStorageError::InvalidClusterId)
        ));
        ConsensusLogStore::open(root.path(), "cluster-a").unwrap();

        let database_path = root
            .path()
            .join("plugins/bmux.cluster/consensus/cluster-a/raft.redb");
        let database = Database::create(database_path).unwrap();
        immediate_write(&database, |transaction| {
            transaction
                .open_table(META_TABLE)?
                .insert(META_CLUSTER_ID, b"cluster-b".as_slice())?;
            Ok(())
        })
        .unwrap();
        drop(database);

        assert!(matches!(
            ConsensusLogStore::open(root.path(), "cluster-a"),
            Err(ConsensusStorageError::ClusterIdMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn state_machine_apply_membership_and_dedup_survive_restart() {
        use bmux_cluster_plugin_api::cluster_types::{
            CommandId, ControlCommand, ControlCommandRequest, ControlWorkflowStatus, WorkspaceId,
        };
        use openraft::storage::RaftStateMachine;

        let root = TempDir::new().unwrap();
        let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let mut state_machine = store.state_machine().unwrap();
        let command = ControlCommand {
            schema_version: 1,
            principal_id: "principal:test".to_string(),
            command_id: CommandId {
                value: uuid::Uuid::from_u128(10),
            },
            issued_at_unix_ms: 50,
            request: ControlCommandRequest::CreateWorkspace {
                workspace_id: WorkspaceId {
                    value: uuid::Uuid::from_u128(20),
                },
                name: Some("workspace".to_string()),
            },
        };
        let membership = Membership::new(
            vec![std::collections::BTreeSet::from([1, 2])],
            std::collections::BTreeMap::from([
                (1, BasicNode::new("one")),
                (2, BasicNode::new("two")),
            ]),
        );
        let entries = [
            Entry {
                log_id: log_id(4, 1),
                payload: EntryPayload::Membership(membership.clone()),
            },
            Entry {
                log_id: log_id(4, 2),
                payload: EntryPayload::Normal(ControlRequest(
                    crate::control_codec::encode_control_command(&command),
                )),
            },
        ];
        let replies = state_machine.apply(entries).await.unwrap();
        assert_eq!(replies.len(), 2);
        assert!(
            state_machine
                .control_state()
                .workspaces
                .contains_key(&uuid::Uuid::from_u128(20))
        );
        drop(state_machine);
        drop(store);

        let reopened = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let mut state_machine = reopened.state_machine().unwrap();
        let (last_applied, stored_membership) = state_machine.applied_state().await.unwrap();
        assert_eq!(last_applied, Some(log_id(4, 2)));
        assert_eq!(stored_membership.membership(), &membership);
        let replay = state_machine
            .apply([Entry {
                log_id: log_id(4, 3),
                payload: EntryPayload::Normal(ControlRequest(
                    crate::control_codec::encode_control_command(&command),
                )),
            }])
            .await
            .unwrap();
        let response = &replay[0].0;
        assert!(response.starts_with(b"BMRES001"));
        assert_eq!(
            state_machine.control_state().revision,
            1,
            "dedup replay must not create another logical mutation"
        );
        let snapshot = state_machine.build_snapshot().await.unwrap();
        let envelope = decode_snapshot_envelope(snapshot.snapshot.get_ref()).unwrap();
        let restored = ControlState::decode_snapshot(&envelope.control_bytes).unwrap();
        assert_eq!(restored, *state_machine.control_state());
        let mut restored = restored;
        assert_eq!(
            restored.apply(&command).workflow_status,
            ControlWorkflowStatus::Complete
        );
    }

    #[tokio::test]
    async fn snapshot_install_is_atomic_and_cluster_bound() {
        use openraft::storage::RaftStateMachine;

        let root = TempDir::new().unwrap();
        let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let mut state_machine = store.state_machine().unwrap();
        let original = state_machine.control_state().clone();
        let mut foreign = ControlState::new("cluster-b");
        foreign.revision = 9;
        let meta = SnapshotMeta {
            last_log_id: Some(log_id(3, 7)),
            last_membership: StoredMembership::default(),
            snapshot_id: "foreign".to_string(),
        };
        assert!(
            state_machine
                .install_snapshot(
                    &meta,
                    Box::new(Cursor::new(foreign.encode_snapshot().unwrap())),
                )
                .await
                .is_err()
        );
        assert_eq!(state_machine.control_state(), &original);

        let mut replacement = ControlState::new("cluster-a");
        replacement.revision = 11;
        state_machine
            .install_snapshot(
                &meta,
                Box::new(Cursor::new(replacement.encode_snapshot().unwrap())),
            )
            .await
            .unwrap();
        drop(state_machine);
        let reopened = store.state_machine().unwrap();
        assert_eq!(reopened.control_state(), &replacement);
    }

    #[tokio::test]
    async fn snapshot_files_are_checksummed_published_and_recovered() {
        use openraft::storage::RaftStateMachine;

        let root = TempDir::new().unwrap();
        let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let mut state_machine = store.state_machine().unwrap();
        assert!(
            state_machine
                .get_current_snapshot()
                .await
                .unwrap()
                .is_none()
        );
        let first = state_machine.build_snapshot().await.unwrap();
        let path = snapshot_path(&state_machine.snapshot_dir, &first.meta.snapshot_id).unwrap();
        assert!(path.is_file());
        assert!(
            !state_machine
                .snapshot_dir
                .join(format!("{}.tmp", first.meta.snapshot_id))
                .exists()
        );
        let current = state_machine.get_current_snapshot().await.unwrap().unwrap();
        assert_eq!(current.meta, first.meta);
        assert_eq!(current.snapshot.get_ref(), first.snapshot.get_ref());
        drop(state_machine);
        drop(store);

        let reopened = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        assert!(reopened.state_machine().is_ok());
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            reopened.state_machine(),
            Err(ConsensusStorageError::CorruptRecord("snapshot checksum"))
        ));
    }

    #[tokio::test]
    async fn failed_snapshot_install_preserves_active_state_and_snapshot() {
        use openraft::storage::RaftStateMachine;

        let root = TempDir::new().unwrap();
        let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let mut state_machine = store.state_machine().unwrap();
        let active = state_machine.build_snapshot().await.unwrap();
        let original = state_machine.control_state().clone();
        let mut bad = active.snapshot.into_inner();
        bad.push(0);
        assert!(
            state_machine
                .install_snapshot(&active.meta, Box::new(Cursor::new(bad)))
                .await
                .is_err()
        );
        assert_eq!(state_machine.control_state(), &original);
        assert_eq!(
            state_machine
                .get_current_snapshot()
                .await
                .unwrap()
                .unwrap()
                .meta,
            active.meta
        );
    }

    #[tokio::test]
    async fn blocking_storage_work_runs_off_the_async_worker() {
        let async_thread = std::thread::current().id();
        let blocking_thread =
            run_storage_blocking(|| Ok::<_, StorageError<u64>>(std::thread::current().id()))
                .await
                .unwrap();
        assert_ne!(async_thread, blocking_thread);

        let mut handles = Vec::new();
        for _ in 0..(STORAGE_BLOCKING_CONCURRENCY * 3) {
            handles.push(tokio::spawn(run_storage_blocking(|| {
                Ok::<_, StorageError<u64>>(std::thread::current().id())
            })));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
    }

    #[test]
    fn durable_codec_round_trips_membership_and_rejects_corruption() {
        let membership = Membership::new(
            vec![std::collections::BTreeSet::from([1, 2])],
            std::collections::BTreeMap::from([
                (1, BasicNode::new("node-one")),
                (2, BasicNode::new("node-two")),
                (3, BasicNode::new("learner-three")),
            ]),
        );
        let entry = Entry {
            log_id: log_id(9, 4),
            payload: EntryPayload::Membership(membership),
        };
        let encoded = encode_entry(&entry).unwrap();
        assert_eq!(decode_entry(&encoded).unwrap(), entry);

        let mut corrupt = encoded;
        corrupt.push(0);
        assert!(matches!(
            decode_entry(&corrupt),
            Err(ConsensusStorageError::CorruptRecord("log entry"))
        ));
    }

    #[test]
    fn corrupt_vote_log_and_state_machine_records_fail_closed() {
        let root = TempDir::new().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mut store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        runtime.block_on(store.save_vote(&Vote::new(2, 1))).unwrap();
        runtime
            .block_on(store.blocking_append([entry(2, 1, b"entry")]))
            .unwrap();
        let mut state_machine = store.state_machine().unwrap();
        runtime
            .block_on(state_machine.apply([Entry {
                log_id: log_id(2, 1),
                payload: EntryPayload::Blank,
            }]))
            .unwrap();

        immediate_write(&store.database, |transaction| {
            transaction
                .open_table(HARD_STATE_TABLE)?
                .insert(HARD_STATE_VOTE, b"bad".as_slice())?;
            Ok(())
        })
        .unwrap();
        assert!(ConsensusLogStore::read_vote(&store).is_err());

        immediate_write(&store.database, |transaction| {
            transaction
                .open_table(LOG_TABLE)?
                .insert(1, b"bad".as_slice())?;
            Ok(())
        })
        .unwrap();
        assert!(runtime.block_on(store.try_get_log_entries(..)).is_err());

        for key in [
            STATE_MACHINE_CONTROL,
            STATE_MACHINE_MEMBERSHIP,
            STATE_MACHINE_LAST_APPLIED,
        ] {
            let clean_root = TempDir::new().unwrap();
            let clean_store = ConsensusLogStore::open(clean_root.path(), "cluster-a").unwrap();
            let mut clean_state = clean_store.state_machine().unwrap();
            runtime
                .block_on(clean_state.apply([Entry {
                    log_id: log_id(3, 1),
                    payload: EntryPayload::Blank,
                }]))
                .unwrap();
            immediate_write(&clean_store.database, |transaction| {
                transaction
                    .open_table(STATE_MACHINE_TABLE)?
                    .insert(key, b"bad".as_slice())?;
                Ok(())
            })
            .unwrap();
            assert!(matches!(
                clean_store.state_machine(),
                Err(ConsensusStorageError::CorruptRecord(_))
            ));
        }
    }

    #[test]
    fn incomplete_or_corrupt_snapshot_metadata_fails_closed() {
        for (key, value) in [
            (META_ACTIVE_SNAPSHOT_ID, b"missing".as_slice()),
            (META_ACTIVE_SNAPSHOT_CHECKSUM, b"bad".as_slice()),
        ] {
            let root = TempDir::new().unwrap();
            let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
            immediate_write(&store.database, |transaction| {
                transaction.open_table(META_TABLE)?.insert(key, value)?;
                Ok(())
            })
            .unwrap();
            assert!(matches!(
                store.state_machine(),
                Err(ConsensusStorageError::CorruptRecord(
                    "snapshot metadata" | "snapshot checksum"
                ))
            ));
        }
    }

    #[test]
    fn corrupt_snapshot_manifest_fails_closed() {
        let root = TempDir::new().unwrap();
        let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        let directory = store.database_path().parent().unwrap().to_path_buf();
        drop(store);
        fs::write(directory.join(SNAPSHOT_INSTALL_MANIFEST), b"bad").unwrap();
        assert!(matches!(
            ConsensusLogStore::open(root.path(), "cluster-a"),
            Err(ConsensusStorageError::CorruptRecord("snapshot manifest"))
        ));
    }

    #[test]
    fn corrupt_metadata_fails_closed() {
        let root = TempDir::new().unwrap();
        let store = ConsensusLogStore::open(root.path(), "cluster-a").unwrap();
        immediate_write(&store.database, |transaction| {
            transaction
                .open_table(META_TABLE)?
                .insert(META_FORMAT_VERSION, b"bad".as_slice())?;
            Ok(())
        })
        .unwrap();
        drop(store);

        assert!(matches!(
            ConsensusLogStore::open(root.path(), "cluster-a"),
            Err(ConsensusStorageError::CorruptRecord("storage version"))
        ));
    }
}
