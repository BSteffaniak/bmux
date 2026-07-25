//! Crash-safe `OpenRaft` storage-v2 log and hard-state persistence.
//!
//! The durable format is intentionally byte-oriented: BMUX owns every key and value
//! encoding instead of coupling consensus compatibility to Rust type layout.

use openraft::storage::{LogFlushed, RaftLogStorage};
use openraft::{
    AnyError, BasicNode, CommittedLeaderId, Entry, EntryPayload, LeaderId, LogId, LogState,
    Membership, RaftLogReader, StorageError, StorageIOError, Vote,
};
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::fs;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const STORAGE_FORMAT_VERSION: u16 = 1;
const CONSENSUS_DB_FILE: &str = "raft.redb";
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const HARD_STATE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("hard_state");
const LOG_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("log");
const META_FORMAT_VERSION: &str = "storage_format_version";
const META_CLUSTER_ID: &str = "cluster_id";
const HARD_STATE_VOTE: &str = "vote";
const HARD_STATE_LAST_PURGED: &str = "last_purged_log_id";

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
        let database_path = directory.join(CONSENSUS_DB_FILE);
        let database = Arc::new(Database::create(&database_path).map_err(redb::Error::from)?);
        initialize_metadata(&database, cluster_id)?;
        Ok(Self {
            database,
            database_path,
        })
    }

    #[must_use]
    pub fn database_path(&self) -> &Path {
        &self.database_path
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
}

impl RaftLogReader<ControlRaftConfig> for ConsensusLogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<ControlRaftConfig>>, StorageError<u64>> {
        let read = self.database.begin_read().map_err(storage_read_error)?;
        let table = read.open_table(LOG_TABLE).map_err(storage_read_error)?;
        let mut entries = Vec::new();
        for row in table.range(range).map_err(storage_read_error)? {
            let (_, value) = row.map_err(storage_read_error)?;
            entries.push(decode_entry(value.value()).map_err(storage_read_error)?);
        }
        Ok(entries)
    }
}

impl RaftLogStorage<ControlRaftConfig> for ConsensusLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<ControlRaftConfig>, StorageError<u64>> {
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

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let encoded = encode_vote(vote);
        immediate_write(&self.database, |transaction| {
            transaction
                .open_table(HARD_STATE_TABLE)?
                .insert(HARD_STATE_VOTE, encoded.as_slice())?;
            Ok(())
        })
        .map_err(storage_write_error)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        Self::read_vote(self).map_err(storage_read_error)
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
        let result = immediate_write(&self.database, |transaction| {
            let mut table = transaction.open_table(LOG_TABLE)?;
            for (index, entry) in &encoded {
                table.insert(*index, entry.as_slice())?;
            }
            Ok(())
        });
        match result {
            Ok(()) => {
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                callback.log_io_completed(Err(std::io::Error::other(message)));
                Err(storage_write_error(error))
            }
        }
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        immediate_write(&self.database, |transaction| {
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
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        let encoded = encode_log_id(&log_id);
        immediate_write(&self.database, |transaction| {
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
        2 => {
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
                        .map_err(|_| ConsensusStorageError::CorruptRecord("log entry"))?;
                    Ok::<_, ConsensusStorageError>((node_id, BasicNode::new(address)))
                })
                .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
            EntryPayload::Membership(Membership::new(configs, nodes))
        }
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
    use tempfile::TempDir;

    fn log_id(term: u64, index: u64) -> LogId<u64> {
        LogId::new(CommittedLeaderId::new(term, 1), index)
    }

    fn entry(term: u64, index: u64, value: &[u8]) -> Entry<ControlRaftConfig> {
        Entry {
            log_id: log_id(term, index),
            payload: EntryPayload::Normal(ControlRequest(value.to_vec())),
        }
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
