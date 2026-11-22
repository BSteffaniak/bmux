//! Durable byte encodings of the owned consensus log, memberships and
//! snapshots, and the one deterministic control-command dispatcher.
//!
//! The format is intentionally byte-oriented: BMUX owns every key and value
//! encoding instead of coupling consensus compatibility to Rust type layout.

use crate::control_codec::{decode_control_command, encode_control_response};
use crate::control_state::ControlState;
use crate::membership::NodeId;
use sha2::{Digest, Sha256};

const SNAPSHOT_FORMAT_VERSION: u16 = 1;
pub(crate) const MAX_SNAPSHOT_FILE_BYTES: usize = 300 * 1024 * 1024;

#[derive(Debug)]
pub enum ConsensusStorageError {
    UnsupportedFormat(u16),
    CorruptRecord(&'static str),
}

impl std::fmt::Display for ConsensusStorageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat(version) => {
                write!(formatter, "unsupported consensus storage format {version}")
            }
            Self::CorruptRecord(record) => write!(formatter, "corrupt consensus {record} record"),
        }
    }
}

impl std::error::Error for ConsensusStorageError {}

// One deterministic dispatcher for every command family. Storage adapters own
// candidate isolation, durable commit and reply release, never this function.
#[derive(Clone, Copy)]
pub(crate) enum ApplicationMembership<'a> {
    /// The committed owned membership the command applies under.
    Owned(&'a crate::consensus_model::StoredMembership),
}

pub(crate) fn apply_control_command(
    control_state: &mut ControlState,
    membership: ApplicationMembership<'_>,
    bytes: &[u8],
) -> Result<Vec<u8>, std::io::Error> {
    let response = if crate::principal_bootstrap::is_bootstrap_command(bytes) {
        let command = crate::principal_bootstrap::BootstrapCommand::decode(bytes)
            .map_err(std::io::Error::other)?;
        let ApplicationMembership::Owned(membership) = membership;
        control_state.apply_owned_bootstrap_command(&command, membership)
    } else if bytes.starts_with(b"BMCAP001") {
        use bmux_cluster_plugin_api::cluster_types::{
            ControlCommandError, ControlCommandResult, ControlResponse, ControlWorkflowStatus,
        };
        let command = crate::capability_publication::PublicationCommand::decode(bytes)
            .map_err(std::io::Error::other)?;
        let ApplicationMembership::Owned(membership) = membership;
        let outcome = control_state.apply_owned_publication(&command, membership);
        let (control_revision, result) = match outcome {
            Ok(revision) => (
                revision,
                ControlCommandResult::Accepted {
                    payload: Vec::new(),
                },
            ),
            Err(reason) => (
                control_state.revision,
                ControlCommandResult::Rejected {
                    error: ControlCommandError::InvalidTransition { reason },
                },
            ),
        };
        ControlResponse {
            schema_version: 1,
            command_id: command.reports[0].command_id.clone(),
            control_revision,
            workflow_status: ControlWorkflowStatus::Complete,
            result,
        }
    } else if crate::control_codec::is_protocol_refresh(bytes) {
        let command = crate::control_codec::ProtocolRefreshCommand::decode(bytes)
            .map_err(std::io::Error::other)?;
        control_state.apply_refresh_command(&command)
    } else if crate::control_codec::is_feature_activation(bytes) {
        let command = crate::control_codec::decode_feature_activation(bytes)
            .map_err(std::io::Error::other)?;
        let ApplicationMembership::Owned(membership) = membership;
        control_state.apply_owned_feature_activation(&command, Some(membership))
    } else {
        let command = decode_control_command(bytes).map_err(std::io::Error::other)?;
        control_state.apply(&command)
    };
    Ok(encode_control_response(&response))
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

fn snapshot_checksum(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

pub(crate) fn encode_owned_snapshot(
    cluster: &str,
    snapshot_id: &str,
    position: crate::consensus_model::LogIdentity,
    membership: &crate::consensus_model::StoredMembership,
    control: &[u8],
) -> Result<Vec<u8>, ConsensusStorageError> {
    let mut stored = DurableWriter::new(*b"BMMEM001");
    stored.boolean(membership.introduced_at.is_some());
    if let Some(identity) = membership.introduced_at {
        encode_owned_log_identity(&mut stored, identity);
    }
    encode_owned_membership_fields(&mut stored, &membership.membership)?;
    let mut writer = DurableWriter::new(*b"BMSNP001");
    writer.u16(SNAPSHOT_FORMAT_VERSION);
    writer.bytes(cluster.as_bytes())?;
    writer.bytes(snapshot_id.as_bytes())?;
    writer.boolean(true);
    encode_owned_log_identity(&mut writer, position);
    writer.bytes(&stored.finish())?;
    writer.bytes(control)?;
    let mut bytes = writer.finish();
    let checksum = snapshot_checksum(&bytes);
    bytes.extend_from_slice(&checksum);
    decode_owned_snapshot(&bytes)?;
    Ok(bytes)
}

fn encode_owned_membership_fields(
    writer: &mut DurableWriter,
    membership: &crate::consensus_model::Membership,
) -> Result<(), ConsensusStorageError> {
    writer.count(membership.configurations().len())?;
    for configuration in membership.configurations() {
        writer.count(configuration.len())?;
        for id in configuration {
            writer.raw(id.as_bytes());
        }
    }
    writer.count(membership.nodes().len())?;
    for (id, endpoint) in membership.nodes() {
        writer.raw(id.as_bytes());
        writer.bytes(endpoint.as_bytes())?;
    }
    Ok(())
}

pub(crate) fn decode_owned_snapshot(
    bytes: &[u8],
) -> Result<(crate::consensus_model::SnapshotDescriptor, Vec<u8>), ConsensusStorageError> {
    decode_owned_snapshot_reader(std::io::Cursor::new(bytes), bytes.len())
}

pub(crate) fn decode_owned_snapshot_reader(
    mut input: impl std::io::Read + std::io::Seek,
    length: usize,
) -> Result<(crate::consensus_model::SnapshotDescriptor, Vec<u8>), ConsensusStorageError> {
    if length > MAX_SNAPSHOT_FILE_BYTES {
        return Err(ConsensusStorageError::CorruptRecord("snapshot size"));
    }
    let payload_len = length
        .checked_sub(32)
        .ok_or(ConsensusStorageError::CorruptRecord("snapshot checksum"))?;
    let invalid = |_| ConsensusStorageError::CorruptRecord("snapshot input");
    input.rewind().map_err(invalid)?;
    let mut hash = Sha256::new();
    let mut remaining = payload_len;
    let mut chunk = [0; 8192];
    while remaining != 0 {
        let size = remaining.min(chunk.len());
        input.read_exact(&mut chunk[..size]).map_err(invalid)?;
        hash.update(&chunk[..size]);
        remaining -= size;
    }
    let mut checksum = [0; 32];
    input.read_exact(&mut checksum).map_err(invalid)?;
    if <[u8; 32]>::from(hash.finalize()) != checksum
        || input.read(&mut chunk[..1]).map_err(invalid)? != 0
    {
        return Err(ConsensusStorageError::CorruptRecord("snapshot checksum"));
    }
    input.rewind().map_err(invalid)?;
    let payload_length = u64::try_from(payload_len)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot size"))?;
    let mut reader = DurableReader::from_reader(
        input.take(payload_length),
        payload_len,
        *b"BMSNP001",
        "snapshot envelope",
    )?;
    let version = reader.u16()?;
    if version != SNAPSHOT_FORMAT_VERSION {
        return Err(ConsensusStorageError::UnsupportedFormat(version));
    }
    let cluster_id = String::from_utf8(reader.bytes()?)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot cluster ID"))?;
    let snapshot_id = String::from_utf8(reader.bytes()?)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot ID"))?;
    validate_snapshot_id(&snapshot_id)?;
    let last_included = if reader.boolean()? {
        Some(decode_owned_log_identity(&mut reader)?)
    } else {
        None
    };
    let membership = decode_owned_stored_membership(&reader.bytes()?)?;
    let descriptor = crate::consensus_model::SnapshotDescriptor {
        format_version: version,
        cluster_id,
        snapshot_id,
        last_included,
        membership,
        total_bytes: u64::try_from(length)
            .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot size"))?,
        checksum,
    };
    descriptor
        .validate_positions()
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot membership position"))?;
    let control_bytes = reader.bytes()?;
    reader.finish()?;
    let control = ControlState::decode_snapshot(&control_bytes)
        .map_err(|_| ConsensusStorageError::CorruptRecord("snapshot control state"))?;
    if control.cluster_id != descriptor.cluster_id {
        return Err(ConsensusStorageError::CorruptRecord(
            "snapshot cluster mismatch",
        ));
    }
    Ok((descriptor, control_bytes))
}

fn validate_node_id(node_id: NodeId) -> Result<NodeId, ConsensusStorageError> {
    node_id
        .public_key()
        .map_err(|_| ConsensusStorageError::CorruptRecord("node ID"))?;
    Ok(node_id)
}

fn decode_owned_stored_membership(
    bytes: &[u8],
) -> Result<Option<crate::consensus_model::StoredMembership>, ConsensusStorageError> {
    let mut reader = DurableReader::new(bytes, *b"BMMEM001", "membership")?;
    let introduced_at = if reader.boolean()? {
        Some(decode_owned_log_identity(&mut reader)?)
    } else {
        None
    };
    let membership = decode_owned_membership(&mut reader)?;
    reader.finish()?;
    match membership {
        Some(membership) => Ok(Some(crate::consensus_model::StoredMembership {
            introduced_at,
            membership,
        })),
        None if introduced_at.is_none() => Ok(None),
        None => Err(ConsensusStorageError::CorruptRecord(
            "empty positioned membership",
        )),
    }
}

fn decode_owned_membership(
    reader: &mut DurableReader<'_>,
) -> Result<Option<crate::consensus_model::Membership>, ConsensusStorageError> {
    let configs = (0..reader.count()?)
        .map(|_| {
            (0..reader.count()?)
                .map(|_| reader.node_id())
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    let nodes = (0..reader.count()?)
        .map(|_| {
            let node_id = reader.node_id()?;
            let address = String::from_utf8(reader.bytes()?)
                .map_err(|_| ConsensusStorageError::CorruptRecord("membership"))?;
            Ok::<_, ConsensusStorageError>((node_id, address))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // The legacy empty installation is a representation, never a voting quorum.
    if configs.is_empty() && nodes.is_empty() {
        return Ok(None);
    }
    crate::consensus_model::Membership::new(configs, nodes)
        .map(Some)
        .map_err(|_| ConsensusStorageError::CorruptRecord("membership"))
}

#[derive(Clone, Copy)]
enum EntryPayloadRef<'a> {
    NoOp,
    Application(&'a [u8]),
    OwnedMembership(&'a crate::consensus_model::Membership),
}

/// Encode an owned entry using the existing durable entry representation.
/// # Errors
/// Returns an error when a count or payload exceeds the format's size limits.
pub fn encode_owned_entry(
    entry: &crate::consensus_model::Entry,
) -> Result<Vec<u8>, ConsensusStorageError> {
    let payload = match &entry.payload {
        crate::consensus_model::EntryPayload::NoOp => EntryPayloadRef::NoOp,
        crate::consensus_model::EntryPayload::Application(bytes) => {
            EntryPayloadRef::Application(bytes)
        }
        crate::consensus_model::EntryPayload::Membership(membership) => {
            EntryPayloadRef::OwnedMembership(membership)
        }
    };
    encode_entry_fields(entry.identity, payload)
}

fn encode_entry_fields(
    identity: crate::consensus_model::LogIdentity,
    payload: EntryPayloadRef<'_>,
) -> Result<Vec<u8>, ConsensusStorageError> {
    let mut writer = DurableWriter::new(*b"BMENT001");
    writer.u64(identity.term.0);
    writer.raw(identity.leader.as_bytes());
    writer.u64(identity.index.0);
    match payload {
        EntryPayloadRef::NoOp => writer.u8(0),
        EntryPayloadRef::Application(bytes) => {
            writer.u8(1);
            writer.bytes(bytes)?;
        }
        EntryPayloadRef::OwnedMembership(membership) => {
            writer.u8(2);
            encode_owned_membership_fields(&mut writer, membership)?;
        }
    }
    Ok(writer.finish())
}

pub(crate) fn decode_owned_entry(
    bytes: &[u8],
) -> Result<crate::consensus_model::Entry, ConsensusStorageError> {
    decode_owned_entry_reader(bytes, bytes.len())
}

pub(crate) fn decode_owned_entry_reader(
    input: impl std::io::Read,
    length: usize,
) -> Result<crate::consensus_model::Entry, ConsensusStorageError> {
    use crate::consensus_model::{Entry, EntryPayload};
    let mut reader = DurableReader::from_reader(input, length, *b"BMENT001", "log entry")?;
    let identity = decode_owned_log_identity(&mut reader)?;
    let payload = match reader.u8()? {
        0 => EntryPayload::NoOp,
        1 => EntryPayload::Application(reader.bytes()?),
        2 => EntryPayload::Membership(decode_owned_membership(&mut reader)?.ok_or(
            ConsensusStorageError::CorruptRecord("empty membership entry"),
        )?),
        _ => return Err(ConsensusStorageError::CorruptRecord("log entry")),
    };
    reader.finish()?;
    Ok(Entry { identity, payload })
}

fn encode_owned_log_identity(
    writer: &mut DurableWriter,
    identity: crate::consensus_model::LogIdentity,
) {
    writer.u64(identity.term.0);
    writer.raw(identity.leader.as_bytes());
    writer.u64(identity.index.0);
}

fn decode_owned_log_identity(
    reader: &mut DurableReader<'_>,
) -> Result<crate::consensus_model::LogIdentity, ConsensusStorageError> {
    Ok(crate::consensus_model::LogIdentity {
        term: crate::consensus_model::Term(reader.u64()?),
        leader: reader.node_id()?,
        index: crate::consensus_model::LogIndex(reader.u64()?),
    })
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

    fn raw(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
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
    input: Box<dyn std::io::Read + 'a>,
    remaining: usize,
    record: &'static str,
}

impl<'a> DurableReader<'a> {
    fn new(
        bytes: &'a [u8],
        magic: [u8; 8],
        record: &'static str,
    ) -> Result<Self, ConsensusStorageError> {
        Self::from_reader(bytes, bytes.len(), magic, record)
    }

    fn from_reader(
        input: impl std::io::Read + 'a,
        length: usize,
        magic: [u8; 8],
        record: &'static str,
    ) -> Result<Self, ConsensusStorageError> {
        let mut reader = Self {
            input: Box::new(input),
            remaining: length,
            record,
        };
        if reader.take(8)? != magic {
            return Err(ConsensusStorageError::CorruptRecord(record));
        }
        Ok(reader)
    }

    fn take(&mut self, count: usize) -> Result<Vec<u8>, ConsensusStorageError> {
        if count > self.remaining {
            return Err(ConsensusStorageError::CorruptRecord(self.record));
        }
        let mut bytes = Vec::new();
        // Bound individual reads and grow only after bytes actually arrive.
        // A forged length on a truncated file cannot preallocate its claim.
        let mut chunk = [0; 8192];
        while bytes.len() < count {
            let size = (count - bytes.len()).min(chunk.len());
            self.input
                .read_exact(&mut chunk[..size])
                .map_err(|_| ConsensusStorageError::CorruptRecord(self.record))?;
            bytes.extend_from_slice(&chunk[..size]);
        }
        self.remaining -= count;
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

    fn node_id(&mut self) -> Result<NodeId, ConsensusStorageError> {
        let bytes = self
            .take(32)?
            .try_into()
            .map_err(|_| ConsensusStorageError::CorruptRecord(self.record))?;
        validate_node_id(NodeId::from_bytes(bytes))
    }

    fn count(&mut self) -> Result<usize, ConsensusStorageError> {
        usize::try_from(self.u32()?).map_err(|_| ConsensusStorageError::CorruptRecord(self.record))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ConsensusStorageError> {
        let count = self.count()?;
        self.take(count)
    }

    fn finish(mut self) -> Result<(), ConsensusStorageError> {
        let mut extra = [0];
        if self.remaining == 0
            && self
                .input
                .read(&mut extra)
                .map_err(|_| ConsensusStorageError::CorruptRecord(self.record))?
                == 0
        {
            Ok(())
        } else {
            Err(ConsensusStorageError::CorruptRecord(self.record))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus_model::{
        Entry, EntryPayload, LogIdentity, LogIndex, Membership, StoredMembership, Term,
    };

    fn identity(term: u64, index: u64) -> LogIdentity {
        LogIdentity {
            term: Term(term),
            leader: NodeId::from(1),
            index: LogIndex(index),
        }
    }

    fn membership(node: NodeId, endpoint: &str) -> Membership {
        Membership::new(vec![vec![node]], vec![(node, endpoint.into())]).unwrap()
    }

    /// The owned entry bytes are the established durable entry layout.
    #[test]
    fn owned_entry_encoder_preserves_wire_bytes() {
        let node = NodeId::from(1);
        let configuration = membership(node, "endpoint");
        for payload in [
            EntryPayload::NoOp,
            EntryPayload::Application(vec![1, 2, 3]),
            EntryPayload::Membership(configuration),
        ] {
            let entry = Entry {
                identity: identity(3, 4),
                payload,
            };
            let mut reference = DurableWriter::new(*b"BMENT001");
            reference.u64(3);
            reference.raw(node.as_bytes());
            reference.u64(4);
            match &entry.payload {
                EntryPayload::NoOp => reference.u8(0),
                EntryPayload::Application(bytes) => {
                    reference.u8(1);
                    reference.bytes(bytes).unwrap();
                }
                EntryPayload::Membership(_) => {
                    reference.u8(2);
                    reference.count(1).unwrap();
                    reference.count(1).unwrap();
                    reference.raw(node.as_bytes());
                    reference.count(1).unwrap();
                    reference.raw(node.as_bytes());
                    reference.bytes(b"endpoint").unwrap();
                }
            }
            let expected = reference.finish();
            assert_eq!(encode_owned_entry(&entry).unwrap(), expected);
            assert_eq!(decode_owned_entry(&expected).unwrap(), entry);
        }
    }

    #[test]
    fn owned_entry_preserves_zero_index_and_rejects_empty_configuration() {
        let entry = Entry {
            identity: identity(1, 0),
            payload: EntryPayload::Application(vec![0, 255, 42]),
        };
        let bytes = encode_owned_entry(&entry).unwrap();
        assert_eq!(decode_owned_entry(&bytes).unwrap(), entry);
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_owned_entry(&trailing).is_err());
        let mut empty = DurableWriter::new(*b"BMENT001");
        encode_owned_log_identity(&mut empty, identity(1, 0));
        empty.u8(2);
        empty.count(0).unwrap();
        empty.count(0).unwrap();
        assert!(decode_owned_entry(&empty.finish()).is_err());
    }

    #[test]
    fn snapshot_rejects_membership_beyond_its_boundary() {
        let node = NodeId::from(17);
        let control = ControlState::new("position-test")
            .encode_snapshot()
            .unwrap();
        let beyond = StoredMembership {
            introduced_at: Some(identity(1, 1)),
            membership: membership(node, "memory://member"),
        };
        assert!(
            encode_owned_snapshot("position-test", "snap", identity(1, 0), &beyond, &control)
                .is_err()
        );
        let covered = StoredMembership {
            introduced_at: Some(identity(1, 0)),
            membership: membership(node, "memory://member"),
        };
        let bytes =
            encode_owned_snapshot("position-test", "snap", identity(1, 0), &covered, &control)
                .unwrap();
        let (descriptor, decoded_control) = decode_owned_snapshot(&bytes).unwrap();
        assert_eq!(descriptor.membership, Some(covered));
        assert_eq!(decoded_control, control);
        let mut corrupt = bytes;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        assert!(decode_owned_snapshot(&corrupt).is_err());
    }

    #[test]
    fn stored_membership_distinguishes_absence_from_positioned_empty_state() {
        let encode = |position: Option<LogIdentity>, nodes: &[(NodeId, &str)]| {
            let mut writer = DurableWriter::new(*b"BMMEM001");
            writer.boolean(position.is_some());
            if let Some(position) = position {
                encode_owned_log_identity(&mut writer, position);
            }
            writer.count(usize::from(!nodes.is_empty())).unwrap();
            if !nodes.is_empty() {
                writer.count(nodes.len()).unwrap();
                for (node, _) in nodes {
                    writer.raw(node.as_bytes());
                }
            }
            writer.count(nodes.len()).unwrap();
            for (node, endpoint) in nodes {
                writer.raw(node.as_bytes());
                writer.bytes(endpoint.as_bytes()).unwrap();
            }
            writer.finish()
        };
        let node = NodeId::from(17);
        for position in [None, Some(identity(1, 0))] {
            let owned = decode_owned_stored_membership(&encode(position, &[(node, "memory://m")]))
                .unwrap()
                .unwrap();
            assert_eq!(owned.introduced_at, position);
        }
        assert!(
            decode_owned_stored_membership(&encode(None, &[]))
                .unwrap()
                .is_none()
        );
        assert!(decode_owned_stored_membership(&encode(Some(identity(1, 0)), &[])).is_err());
    }

    #[test]
    fn durable_membership_rejects_duplicate_identities() {
        let node = NodeId::from(17);
        for (voter_count, record_count) in [(2, 1), (1, 2), (1, 0)] {
            let mut writer = DurableWriter::new(*b"BMMEM001");
            writer.boolean(false);
            writer.count(1).unwrap();
            writer.count(voter_count).unwrap();
            for _ in 0..voter_count {
                writer.raw(node.as_bytes());
            }
            writer.count(record_count).unwrap();
            for _ in 0..record_count {
                writer.raw(node.as_bytes());
                writer.bytes(b"memory://member").unwrap();
            }
            assert!(decode_owned_stored_membership(&writer.finish()).is_err());
        }
    }

    #[test]
    fn owned_identity_round_trips_extremes() {
        let node = NodeId::from(17);
        for (term, index) in [(0, 0), (u64::MAX, u64::MAX)] {
            let mut writer = DurableWriter::new(*b"BMLOGID1");
            let expected = LogIdentity {
                term: Term(term),
                leader: node,
                index: LogIndex(index),
            };
            encode_owned_log_identity(&mut writer, expected);
            let bytes = writer.finish();
            let mut reader = DurableReader::new(&bytes, *b"BMLOGID1", "log identity").unwrap();
            assert_eq!(decode_owned_log_identity(&mut reader).unwrap(), expected);
            reader.finish().unwrap();
        }
    }
}
