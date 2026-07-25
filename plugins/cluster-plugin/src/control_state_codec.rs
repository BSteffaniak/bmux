use crate::control_codec::{
    CodecError, Reader, Writer, decode_state_response, encode_state_response,
};
use bmux_cluster_plugin_api::cluster_types::{
    ClusterMember, LogicalPaneRecord, LogicalWindowRecord,
};

use super::{
    CONTROL_SCHEMA_VERSION, ControlState, DedupKey, DedupRecord, MAX_SNAPSHOT_BYTES,
    MAX_SNAPSHOT_ITEMS, SNAPSHOT_MAGIC, StateCodecError,
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

pub(super) fn encode_snapshot(state: &ControlState) -> Result<Vec<u8>, StateCodecError> {
    if state.schema_version != CONTROL_SCHEMA_VERSION {
        return Err(StateCodecError::UnsupportedSchema(state.schema_version));
    }
    let mut writer = Writer::default();
    writer.raw(SNAPSHOT_MAGIC);
    writer.u16(state.schema_version);
    writer.string(&state.cluster_id);
    writer.u64(state.revision);

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

    write_count(&mut writer, state.windows.len())?;
    for (key, window) in &state.windows {
        if key != &window.window_id.value {
            return Err(StateCodecError::InvalidState("window map key mismatch"));
        }
        writer.uuid(*key);
        writer.encode_state_window(window);
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
        encode_state_response(&mut writer, &record.response);
    }

    let bytes = writer.into_bytes();
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(StateCodecError::LimitExceeded("bytes"));
    }
    Ok(bytes)
}

pub(super) fn decode_snapshot(bytes: &[u8]) -> Result<ControlState, StateCodecError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(StateCodecError::LimitExceeded("bytes"));
    }
    let mut reader = Reader::new(bytes);
    if reader.take(SNAPSHOT_MAGIC.len())? != SNAPSHOT_MAGIC {
        return Err(StateCodecError::InvalidMagic);
    }
    let schema_version = reader.u16()?;
    if schema_version != CONTROL_SCHEMA_VERSION {
        return Err(StateCodecError::UnsupportedSchema(schema_version));
    }
    let cluster_id = reader.string()?;
    let revision = reader.u64()?;

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
    let windows = read_map(&mut reader, |reader| {
        let key = reader.uuid()?;
        let window = reader.decode_state_window()?;
        if key != window.window_id.value {
            return Err(StateCodecError::InvalidState("window map key mismatch"));
        }
        Ok((key, window))
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
        let response = decode_state_response(reader)?;
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
                response,
            },
        ))
    })?;
    reader.finish()?;

    let state = ControlState {
        schema_version,
        cluster_id,
        revision,
        members,
        workspaces,
        windows,
        panes,
        dedup,
    };
    validate_references(&state)?;
    Ok(state)
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
    for LogicalWindowRecord { workspace_id, .. } in state.windows.values() {
        if !state.workspaces.contains_key(&workspace_id.value) {
            return Err(StateCodecError::InvalidState(
                "window references missing workspace",
            ));
        }
    }
    for LogicalPaneRecord {
        workspace_id,
        window_id,
        execution,
        ..
    } in state.panes.values()
    {
        let Some(window) = state.windows.get(&window_id.value) else {
            return Err(StateCodecError::InvalidState(
                "pane references missing window",
            ));
        };
        if window.workspace_id.value != workspace_id.value
            || !state.workspaces.contains_key(&workspace_id.value)
        {
            return Err(StateCodecError::InvalidState(
                "pane workspace/window mismatch",
            ));
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
