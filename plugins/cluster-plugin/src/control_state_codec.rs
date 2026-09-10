use crate::control_codec::{
    CodecError, Reader, Writer, decode_state_response, encode_state_response,
};
use bmux_cluster_plugin_api::cluster_types::{ClusterMember, LogicalPaneRecord, LogicalTabRecord};

use super::{
    CONTROL_CODEC_VERSION, CONTROL_SCHEMA_VERSION, ControlState, DedupKey, DedupRecord,
    FeatureDedupRecord, LEGACY_SNAPSHOT_FORMAT_VERSION, LEGACY_SNAPSHOT_MAGIC, MAX_SNAPSHOT_BYTES,
    MAX_SNAPSHOT_ITEMS, PREVIOUS_SNAPSHOT_FORMAT_VERSION, PREVIOUS_SNAPSHOT_MAGIC,
    SNAPSHOT_FORMAT_VERSION, SNAPSHOT_MAGIC, StateCodecError,
};

impl From<CodecError> for StateCodecError {
    fn from(error: CodecError) -> Self {
        match error {
            CodecError::Truncated => Self::Truncated,
            CodecError::InvalidMagic => Self::InvalidMagic,
            CodecError::UnsupportedSchema(version) => Self::UnsupportedSchema(version),
            CodecError::InvalidBoolean(value) => Self::InvalidBoolean(value),
            CodecError::InvalidUtf8 => Self::InvalidUtf8,
            CodecError::LimitExceeded(name) => Self::LimitExceeded(name),
            CodecError::TrailingBytes => Self::TrailingBytes,
            CodecError::InvalidTag { .. } => Self::InvalidState("invalid canonical enum tag"),
        }
    }
}

fn encode_refresh_snapshot(state: &ControlState) -> Result<Vec<u8>, StateCodecError> {
    if state.refresh_outcomes.len() > 1024 {
        return Err(StateCodecError::LimitExceeded("refresh outcomes"));
    }
    let mut base = state.clone();
    base.refresh_outcomes.clear();
    let mut writer = Writer::default();
    writer.raw(b"BMSTA005");
    writer.bytes(&encode_snapshot(&base)?);
    write_count(&mut writer, state.refresh_outcomes.len())?;
    for (id, (fingerprint, revision)) in &state.refresh_outcomes {
        if id.is_nil() || *revision == 0 || *revision > state.revision {
            return Err(StateCodecError::InvalidState("invalid refresh outcome"));
        }
        writer.uuid(*id);
        writer.raw(fingerprint);
        writer.u64(*revision);
    }
    let bytes = writer.into_bytes();
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(StateCodecError::LimitExceeded("bytes"));
    }
    Ok(bytes)
}

pub(super) fn encode_snapshot(state: &ControlState) -> Result<Vec<u8>, StateCodecError> {
    if !state.publication_history.is_empty() {
        if state.publication_history.len() > 64 {
            return Err(StateCodecError::LimitExceeded("publication history"));
        }
        let mut base = state.clone();
        base.publication_history.clear();
        let mut w = Writer::default();
        w.raw(b"BMSTA006");
        w.bytes(&encode_snapshot(&base)?);
        w.u16(
            u16::try_from(state.publication_history.len())
                .map_err(|_| StateCodecError::LimitExceeded("publication history"))?,
        );
        for (command, revision) in &state.publication_history {
            w.bytes(
                &command
                    .encode()
                    .map_err(|_| StateCodecError::InvalidState("invalid publication"))?,
            );
            w.u64(*revision);
        }
        let bytes = w.into_bytes();
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(StateCodecError::LimitExceeded("bytes"));
        }
        return Ok(bytes);
    }
    if !state.refresh_outcomes.is_empty() {
        return encode_refresh_snapshot(state);
    }
    encode_base_snapshot(state)
}

fn encode_base_snapshot(state: &ControlState) -> Result<Vec<u8>, StateCodecError> {
    if state.schema_version != CONTROL_SCHEMA_VERSION {
        return Err(StateCodecError::UnsupportedSchema(state.schema_version));
    }
    let mut writer = Writer::default();
    let advanced = state.read_schema_floor != CONTROL_SCHEMA_VERSION
        || state.write_schema_floor != CONTROL_SCHEMA_VERSION
        || !state.activated_features.is_empty();
    let bootstrap_format = state.principal_bootstrap.is_some();
    if bootstrap_format {
        if !advanced || state.read_schema_floor < 3 || state.write_schema_floor < 3 {
            return Err(StateCodecError::InvalidState(
                "bootstrap requires schema floor 3",
            ));
        }
        writer.raw(b"BMSTA004");
        writer.u16(4);
    } else if advanced {
        writer.raw(SNAPSHOT_MAGIC);
        writer.u16(SNAPSHOT_FORMAT_VERSION);
    } else {
        writer.raw(PREVIOUS_SNAPSHOT_MAGIC);
        writer.u16(PREVIOUS_SNAPSHOT_FORMAT_VERSION);
    }
    writer.u16(CONTROL_CODEC_VERSION);
    writer.u16(state.schema_version);
    writer.string(&state.cluster_id);
    writer.u64(state.revision);
    if advanced {
        writer.u16(state.read_schema_floor);
        writer.u16(state.write_schema_floor);
        write_count(&mut writer, state.activated_features.len())?;
        for feature in &state.activated_features {
            writer.string(feature);
        }
    }

    write_count(&mut writer, state.members.len())?;
    for (key, member) in &state.members {
        if key != &member.node_id {
            return Err(StateCodecError::InvalidState("member map key mismatch"));
        }
        writer.string(key);
        writer.encode_state_member(member);
    }

    write_count(&mut writer, state.workspaces.len())?;
    for (key, workspace) in &state.workspaces {
        if key != &workspace.workspace_id.value {
            return Err(StateCodecError::InvalidState("workspace map key mismatch"));
        }
        writer.uuid(*key);
        writer.encode_state_workspace(workspace);
    }

    write_count(&mut writer, state.tabs.len())?;
    for (key, tab) in &state.tabs {
        if key != &tab.tab_id.value {
            return Err(StateCodecError::InvalidState("tab map key mismatch"));
        }
        writer.uuid(*key);
        writer.encode_state_tab(tab);
    }

    write_count(&mut writer, state.panes.len())?;
    for (key, pane) in &state.panes {
        if key != &pane.pane_id.value {
            return Err(StateCodecError::InvalidState("pane map key mismatch"));
        }
        writer.uuid(*key);
        writer.encode_state_pane(pane);
    }

    write_count(&mut writer, state.dedup.len())?;
    for (key, record) in &state.dedup {
        writer.string(&key.principal_id);
        writer.uuid(key.command_id);
        writer.raw(&record.fingerprint);
        writer.u64(record.issued_at_unix_ms);
        writer.bytes(&crate::control_codec::encode_control_command(
            &record.command,
        ));
        encode_state_response(&mut writer, &record.response);
    }

    if advanced {
        write_count(&mut writer, state.feature_dedup.len())?;
        for (key, record) in &state.feature_dedup {
            writer.string(&key.principal_id);
            writer.uuid(key.command_id);
            writer.raw(&record.fingerprint);
            writer.u64(record.issued_at_unix_ms);
            writer.bytes(&crate::control_codec::encode_feature_activation(
                &record.command,
            ));
            encode_state_response(&mut writer, &record.response);
        }
    }

    if let Some(record) = &state.principal_bootstrap {
        encode_bootstrap_record(&mut writer, record, state.revision)?;
    }
    let bytes = writer.into_bytes();
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(StateCodecError::LimitExceeded("bytes"));
    }
    Ok(bytes)
}

#[allow(clippy::too_many_lines)]
pub(super) fn decode_snapshot(bytes: &[u8]) -> Result<ControlState, StateCodecError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(StateCodecError::LimitExceeded("bytes"));
    }
    let mut reader = Reader::new(bytes);
    let magic = reader.take(SNAPSHOT_MAGIC.len())?;
    if magic == b"BMSTA006" {
        let base = reader.bytes()?;
        if base.starts_with(b"BMSTA006") {
            return Err(StateCodecError::InvalidState("nested publication snapshot"));
        }
        let mut state = decode_snapshot(&base)?;
        let count = reader.u16()?;
        if count == 0 || count > 64 {
            return Err(StateCodecError::LimitExceeded("publication history"));
        }
        let mut last_revision = 0;
        let mut ids = std::collections::BTreeSet::new();
        let mut revisions = std::collections::BTreeMap::new();
        for _ in 0..count {
            let command =
                crate::capability_publication::PublicationCommand::decode(&reader.bytes()?)
                    .map_err(|_| StateCodecError::InvalidState("invalid publication command"))?;
            let revision = reader.u64()?;
            if revision <= last_revision || revision > state.revision {
                return Err(StateCodecError::InvalidState(
                    "invalid publication revision",
                ));
            }
            for report in &command.reports {
                let expected = revisions.entry(report.node_id.clone()).or_insert(0_u64);
                if report.cluster_id != state.cluster_id
                    || report.expected_report_revision != *expected
                    || !ids.insert(report.command_id.value)
                {
                    return Err(StateCodecError::InvalidState("invalid publication history"));
                }
                *expected = expected
                    .checked_add(1)
                    .ok_or(StateCodecError::InvalidState("report revision overflow"))?;
            }
            last_revision = revision;
            state.publication_history.push((command, revision));
        }
        reader.finish()?;
        return Ok(state);
    }
    if magic == b"BMSTA005" {
        let base = reader.bytes()?;
        if base.starts_with(b"BMSTA005") || base.starts_with(b"BMSTA006") {
            return Err(StateCodecError::InvalidState("nested refresh snapshot"));
        }
        let mut state = decode_snapshot(&base)?;
        state.refresh_outcomes = read_map(&mut reader, |reader| {
            let id = reader.uuid()?;
            let fingerprint = reader
                .take(32)?
                .try_into()
                .expect("exact fingerprint length");
            let revision = reader.u64()?;
            if id.is_nil() || revision == 0 || revision > state.revision {
                return Err(StateCodecError::InvalidState("invalid refresh outcome"));
            }
            Ok((id, (fingerprint, revision)))
        })?;
        if state.refresh_outcomes.is_empty() || state.refresh_outcomes.len() > 1024 {
            return Err(StateCodecError::InvalidState(
                "invalid refresh outcome count",
            ));
        }
        reader.finish()?;
        return Ok(state);
    }
    let bootstrap_format = magic == b"BMSTA004";
    let (schema_version, advanced) = if magic == SNAPSHOT_MAGIC || bootstrap_format {
        let format_version = reader.u16()?;
        let expected_format = if bootstrap_format {
            4
        } else {
            SNAPSHOT_FORMAT_VERSION
        };
        if format_version != expected_format {
            return Err(StateCodecError::UnsupportedSnapshotFormat(format_version));
        }
        let codec_version = reader.u16()?;
        if codec_version != CONTROL_CODEC_VERSION {
            return Err(StateCodecError::UnsupportedCodec(codec_version));
        }
        (reader.u16()?, true)
    } else if magic == PREVIOUS_SNAPSHOT_MAGIC {
        let format_version = reader.u16()?;
        if format_version != PREVIOUS_SNAPSHOT_FORMAT_VERSION {
            return Err(StateCodecError::UnsupportedSnapshotFormat(format_version));
        }
        let codec_version = reader.u16()?;
        if codec_version != CONTROL_CODEC_VERSION {
            return Err(StateCodecError::UnsupportedCodec(codec_version));
        }
        (reader.u16()?, false)
    } else if magic == LEGACY_SNAPSHOT_MAGIC {
        (
            migrate_legacy_snapshot_header(&mut reader, LEGACY_SNAPSHOT_FORMAT_VERSION)?,
            false,
        )
    } else {
        return Err(StateCodecError::InvalidMagic);
    };
    if schema_version != CONTROL_SCHEMA_VERSION {
        return Err(StateCodecError::UnsupportedSchema(schema_version));
    }
    let cluster_id = reader.string()?;
    let revision = reader.u64()?;
    let (read_schema_floor, write_schema_floor, activated_features) = if advanced {
        let read_schema_floor = reader.u16()?;
        let write_schema_floor = reader.u16()?;
        let count = read_count(&mut reader)?;
        let mut features = std::collections::BTreeSet::new();
        for _ in 0..count {
            let feature = reader.string()?;
            if feature.is_empty() || !features.insert(feature) {
                return Err(StateCodecError::InvalidState(
                    "activated feature list is empty or duplicated",
                ));
            }
        }
        (read_schema_floor, write_schema_floor, features)
    } else {
        (
            CONTROL_SCHEMA_VERSION,
            CONTROL_SCHEMA_VERSION,
            std::collections::BTreeSet::new(),
        )
    };

    let members = read_map(&mut reader, |reader| {
        let key = reader.string()?;
        let member = reader.decode_state_member()?;
        if key != member.node_id {
            return Err(StateCodecError::InvalidState("member map key mismatch"));
        }
        Ok((key, member))
    })?;
    let workspaces = read_map(&mut reader, |reader| {
        let key = reader.uuid()?;
        let workspace = reader.decode_state_workspace()?;
        if key != workspace.workspace_id.value {
            return Err(StateCodecError::InvalidState("workspace map key mismatch"));
        }
        Ok((key, workspace))
    })?;
    let tabs = read_map(&mut reader, |reader| {
        let key = reader.uuid()?;
        let tab = reader.decode_state_tab()?;
        if key != tab.tab_id.value {
            return Err(StateCodecError::InvalidState("tab map key mismatch"));
        }
        Ok((key, tab))
    })?;
    let panes = read_map(&mut reader, |reader| {
        let key = reader.uuid()?;
        let pane = reader.decode_state_pane()?;
        if key != pane.pane_id.value {
            return Err(StateCodecError::InvalidState("pane map key mismatch"));
        }
        Ok((key, pane))
    })?;
    let dedup = read_map(&mut reader, |reader| {
        let key = DedupKey {
            principal_id: reader.string()?,
            command_id: reader.uuid()?,
        };
        let fingerprint = reader
            .take(32)?
            .try_into()
            .expect("exact fingerprint length");
        let issued_at_unix_ms = reader.u64()?;
        let command = crate::control_codec::decode_control_command(&reader.bytes()?)?;
        let response = decode_state_response(reader)?;
        if command.principal_id != key.principal_id || command.command_id.value != key.command_id {
            return Err(StateCodecError::InvalidState(
                "dedup command identity mismatch",
            ));
        }
        if response.command_id.value != key.command_id {
            return Err(StateCodecError::InvalidState(
                "dedup response command mismatch",
            ));
        }
        Ok((
            key,
            DedupRecord {
                fingerprint,
                issued_at_unix_ms,
                command,
                response,
            },
        ))
    })?;
    let feature_dedup = if advanced {
        read_map(&mut reader, |reader| {
            let key = DedupKey {
                principal_id: reader.string()?,
                command_id: reader.uuid()?,
            };
            let fingerprint = reader
                .take(32)?
                .try_into()
                .expect("exact fingerprint length");
            let issued_at_unix_ms = reader.u64()?;
            let command = crate::control_codec::decode_feature_activation(&reader.bytes()?)?;
            let response = decode_state_response(reader)?;
            if command.principal_id != key.principal_id
                || command.command_id.value != key.command_id
                || response.command_id.value != key.command_id
            {
                return Err(StateCodecError::InvalidState(
                    "feature dedup identity mismatch",
                ));
            }
            Ok((
                key,
                FeatureDedupRecord {
                    fingerprint,
                    issued_at_unix_ms,
                    command,
                    response,
                },
            ))
        })?
    } else {
        std::collections::BTreeMap::new()
    };
    let principal_bootstrap = if bootstrap_format {
        if read_schema_floor < 3 || write_schema_floor < 3 {
            return Err(StateCodecError::InvalidState(
                "bootstrap requires schema floor 3",
            ));
        }
        let record = super::PrincipalBootstrapRecord {
            principal_id: reader.uuid()?,
            public_key: reader.string()?,
            command_id: reader.uuid()?,
            statement_fingerprint: reader
                .take(32)?
                .try_into()
                .expect("exact fingerprint length"),
            committed_revision: reader.u64()?,
        };
        validate_bootstrap_record(&record, revision)?;
        Some(record)
    } else {
        None
    };
    reader.finish()?;

    let state = ControlState {
        schema_version,
        cluster_id,
        revision,
        read_schema_floor,
        write_schema_floor,
        activated_features,
        members,
        workspaces,
        tabs,
        panes,
        principal_bootstrap,
        publication_history: Vec::new(),
        refresh_outcomes: std::collections::BTreeMap::new(),
        dedup,
        feature_dedup,
    };
    validate_references(&state)?;
    Ok(state)
}

fn encode_bootstrap_record(
    writer: &mut Writer,
    record: &super::PrincipalBootstrapRecord,
    revision: u64,
) -> Result<(), StateCodecError> {
    validate_bootstrap_record(record, revision)?;
    writer.uuid(record.principal_id);
    writer.string(&record.public_key);
    writer.uuid(record.command_id);
    writer.raw(&record.statement_fingerprint);
    writer.u64(record.committed_revision);
    Ok(())
}

fn validate_bootstrap_record(
    record: &super::PrincipalBootstrapRecord,
    revision: u64,
) -> Result<(), StateCodecError> {
    let key: iroh::PublicKey = record
        .public_key
        .parse()
        .map_err(|_| StateCodecError::InvalidState("invalid principal key"))?;
    if key.to_string() != record.public_key
        || record.principal_id.is_nil()
        || record.command_id.is_nil()
        || record.committed_revision == 0
        || record.committed_revision > revision
    {
        return Err(StateCodecError::InvalidState("invalid bootstrap record"));
    }
    Ok(())
}

fn migrate_legacy_snapshot_header(
    reader: &mut Reader<'_>,
    format_version: u16,
) -> Result<u16, StateCodecError> {
    match format_version {
        LEGACY_SNAPSHOT_FORMAT_VERSION => Ok(reader.u16()?),
        version => Err(StateCodecError::UnsupportedSnapshotFormat(version)),
    }
}

fn write_count(writer: &mut Writer, count: usize) -> Result<(), StateCodecError> {
    if count > MAX_SNAPSHOT_ITEMS {
        return Err(StateCodecError::LimitExceeded("item count"));
    }
    writer.u32(u32::try_from(count).map_err(|_| StateCodecError::LimitExceeded("item count"))?);
    Ok(())
}

fn read_count(reader: &mut Reader<'_>) -> Result<usize, StateCodecError> {
    let count = usize::try_from(reader.u32()?).expect("u32 must fit usize");
    if count > MAX_SNAPSHOT_ITEMS {
        return Err(StateCodecError::LimitExceeded("item count"));
    }
    Ok(count)
}

fn read_map<K: Ord, V>(
    reader: &mut Reader<'_>,
    mut read: impl FnMut(&mut Reader<'_>) -> Result<(K, V), StateCodecError>,
) -> Result<std::collections::BTreeMap<K, V>, StateCodecError> {
    let count = read_count(reader)?;
    let mut result = std::collections::BTreeMap::new();
    for _ in 0..count {
        let (key, value) = read(reader)?;
        if result.insert(key, value).is_some() {
            return Err(StateCodecError::InvalidState("duplicate map key"));
        }
    }
    Ok(result)
}

fn validate_references(state: &ControlState) -> Result<(), StateCodecError> {
    if state.read_schema_floor < CONTROL_SCHEMA_VERSION
        || state.write_schema_floor < CONTROL_SCHEMA_VERSION
        || state.read_schema_floor > state.write_schema_floor
        || (state.write_schema_floor == CONTROL_SCHEMA_VERSION
            && !state.activated_features.is_empty())
    {
        return Err(StateCodecError::InvalidState(
            "control feature floors are inconsistent",
        ));
    }
    for LogicalTabRecord { workspace_id, .. } in state.tabs.values() {
        if !state.workspaces.contains_key(&workspace_id.value) {
            return Err(StateCodecError::InvalidState(
                "tab references missing workspace",
            ));
        }
    }
    for LogicalPaneRecord {
        workspace_id,
        tab_id,
        execution,
        ..
    } in state.panes.values()
    {
        let Some(tab) = state.tabs.get(&tab_id.value) else {
            return Err(StateCodecError::InvalidState("pane references missing tab"));
        };
        if tab.workspace_id.value != workspace_id.value
            || !state.workspaces.contains_key(&workspace_id.value)
        {
            return Err(StateCodecError::InvalidState("pane workspace/tab mismatch"));
        }
        if execution
            .as_ref()
            .is_some_and(|assignment| assignment.generation == 0)
        {
            return Err(StateCodecError::InvalidState(
                "execution generation must be positive",
            ));
        }
    }
    for ClusterMember { cluster_id, .. } in state.members.values() {
        if cluster_id != &state.cluster_id {
            return Err(StateCodecError::InvalidState(
                "member belongs to another cluster",
            ));
        }
    }
    Ok(())
}
