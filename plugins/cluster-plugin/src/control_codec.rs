use bmux_cluster_plugin_api::cluster_types::{
    ClusterConsensusRole, ClusterMember, ClusterMemberState, ClusterNegotiatedProtocol,
    ClusterNodeCapabilities, CommandId, ControlCommand, ControlCommandError, ControlCommandRequest,
    ControlCommandResult, ControlResourceKind, ControlResponse, ControlWorkflowStatus,
    ExecutionAssignment, LogicalPaneRecord, LogicalWindowRecord, PaneAvailability,
    PaneRestartPolicy, PlacementIntent, PlacementLabel, WorkspaceId, WorkspaceRecord,
};
use sha2::{Digest, Sha256};

const CODEC_MAGIC: &[u8; 8] = b"BMCMD001";
const FEATURE_COMMAND_MAGIC: &[u8; 8] = b"BMCMD002";
const CONTROL_SCHEMA_VERSION: u16 = 1;
const FEATURE_SCHEMA_VERSION: u16 = 2;
const MAX_STRING_BYTES: usize = 65_536;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LIST_ITEMS: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    Truncated,
    InvalidMagic,
    UnsupportedSchema(u16),
    InvalidTag { type_name: &'static str, tag: u16 },
    InvalidBoolean(u8),
    InvalidUtf8,
    LimitExceeded(&'static str),
    TrailingBytes,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("canonical command is truncated"),
            Self::InvalidMagic => formatter.write_str("canonical command magic is invalid"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported control schema version {version}")
            }
            Self::InvalidTag { type_name, tag } => {
                write!(formatter, "invalid {type_name} tag {tag}")
            }
            Self::InvalidBoolean(value) => write!(formatter, "invalid canonical boolean {value}"),
            Self::InvalidUtf8 => formatter.write_str("canonical string is not UTF-8"),
            Self::LimitExceeded(name) => write!(formatter, "canonical {name} exceeds its limit"),
            Self::TrailingBytes => formatter.write_str("canonical command has trailing bytes"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Computes the canonical request-only deduplication fingerprint.
///
/// # Panics
///
/// Panics if a typed field exceeds the schema-defined canonical bounds.
#[must_use]
pub fn request_fingerprint(command: &ControlCommand) -> [u8; 32] {
    validate_command(command).expect("typed control command must satisfy canonical limits");
    let mut writer = Writer::default();
    encode_request(&mut writer, &command.request);
    Sha256::digest(writer.bytes).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureActivationCommand {
    pub principal_id: String,
    pub command_id: CommandId,
    pub issued_at_unix_ms: u64,
    pub expected_control_revision: u64,
    pub read_schema_floor: u16,
    pub write_schema_floor: u16,
    pub feature: String,
}

#[must_use]
/// Encodes one feature activation into its canonical durable representation.
///
/// # Panics
///
/// Panics if a typed principal or feature name exceeds the schema-defined
/// canonical bounds. Callers must reject such a command before proposal.
pub fn encode_feature_activation(command: &FeatureActivationCommand) -> Vec<u8> {
    validate_string(&command.principal_id).expect("feature principal must be bounded");
    validate_string(&command.feature).expect("feature name must be bounded");
    let mut writer = Writer::default();
    writer.raw(FEATURE_COMMAND_MAGIC);
    writer.u16(FEATURE_SCHEMA_VERSION);
    writer.string(&command.principal_id);
    writer.uuid(command.command_id.value);
    writer.u64(command.issued_at_unix_ms);
    writer.u64(command.expected_control_revision);
    writer.u16(command.read_schema_floor);
    writer.u16(command.write_schema_floor);
    writer.string(&command.feature);
    writer.into_bytes()
}

/// Decodes and validates one canonical durable feature activation.
///
/// # Errors
///
/// Returns an error for malformed, oversized, trailing, or unsupported data.
pub fn decode_feature_activation(bytes: &[u8]) -> Result<FeatureActivationCommand, CodecError> {
    let mut reader = Reader::new(bytes);
    if reader.take(FEATURE_COMMAND_MAGIC.len())? != FEATURE_COMMAND_MAGIC {
        return Err(CodecError::InvalidMagic);
    }
    let schema_version = reader.u16()?;
    if schema_version != FEATURE_SCHEMA_VERSION {
        return Err(CodecError::UnsupportedSchema(schema_version));
    }
    let command = FeatureActivationCommand {
        principal_id: reader.string()?,
        command_id: CommandId {
            value: reader.uuid()?,
        },
        issued_at_unix_ms: reader.u64()?,
        expected_control_revision: reader.u64()?,
        read_schema_floor: reader.u16()?,
        write_schema_floor: reader.u16()?,
        feature: reader.string()?,
    };
    reader.finish()?;
    validate_string(&command.principal_id)?;
    validate_string(&command.feature)?;
    Ok(command)
}

#[must_use]
pub fn is_feature_activation(bytes: &[u8]) -> bool {
    bytes.starts_with(FEATURE_COMMAND_MAGIC)
}

/// Encodes a typed control command into the canonical durable representation.
///
/// # Panics
///
/// Panics if a typed field exceeds the schema-defined canonical bounds. Such a
/// command must be rejected before consensus proposal.
#[must_use]
pub fn encode_control_command(command: &ControlCommand) -> Vec<u8> {
    validate_command(command).expect("typed control command must satisfy canonical limits");
    let mut writer = Writer::default();
    writer.raw(CODEC_MAGIC);
    writer.u16(command.schema_version);
    writer.string(&command.principal_id);
    writer.uuid(command.command_id.value);
    writer.u64(command.issued_at_unix_ms);
    encode_request(&mut writer, &command.request);
    writer.bytes
}

/// Decodes one complete canonical control command.
///
/// # Errors
///
/// Returns [`CodecError`] when the envelope is truncated, non-canonical,
/// exceeds a schema limit, or uses an unsupported schema/tag.
pub fn decode_control_command(bytes: &[u8]) -> Result<ControlCommand, CodecError> {
    let mut reader = Reader::new(bytes);
    if reader.take(CODEC_MAGIC.len())? != CODEC_MAGIC {
        return Err(CodecError::InvalidMagic);
    }
    let schema_version = reader.u16()?;
    if schema_version != CONTROL_SCHEMA_VERSION {
        return Err(CodecError::UnsupportedSchema(schema_version));
    }
    let command = ControlCommand {
        schema_version,
        principal_id: reader.string()?,
        command_id: CommandId {
            value: reader.uuid()?,
        },
        issued_at_unix_ms: reader.u64()?,
        request: decode_request(&mut reader)?,
    };
    reader.finish()?;
    Ok(command)
}

pub(crate) fn encode_state_response(writer: &mut Writer, response: &ControlResponse) {
    writer.u16(response.schema_version);
    writer.uuid(response.command_id.value);
    writer.u64(response.control_revision);
    writer.u16(match response.workflow_status {
        ControlWorkflowStatus::Complete => 1,
        ControlWorkflowStatus::Pending => 2,
    });
    match &response.result {
        ControlCommandResult::Accepted { payload } => {
            writer.u16(1);
            writer.bytes(payload);
        }
        ControlCommandResult::Rejected { error } => {
            writer.u16(2);
            encode_error(writer, error);
        }
    }
}

pub(crate) fn decode_state_response(
    reader: &mut Reader<'_>,
) -> Result<ControlResponse, CodecError> {
    let schema_version = reader.u16()?;
    if !(CONTROL_SCHEMA_VERSION..=FEATURE_SCHEMA_VERSION).contains(&schema_version) {
        return Err(CodecError::UnsupportedSchema(schema_version));
    }
    let command_id = CommandId {
        value: reader.uuid()?,
    };
    let control_revision = reader.u64()?;
    let workflow_status = match reader.u16()? {
        1 => ControlWorkflowStatus::Complete,
        2 => ControlWorkflowStatus::Pending,
        tag => {
            return Err(CodecError::InvalidTag {
                type_name: "workflow status",
                tag,
            });
        }
    };
    let result = match reader.u16()? {
        1 => ControlCommandResult::Accepted {
            payload: reader.bytes()?,
        },
        2 => ControlCommandResult::Rejected {
            error: decode_error(reader)?,
        },
        tag => {
            return Err(CodecError::InvalidTag {
                type_name: "command result",
                tag,
            });
        }
    };
    Ok(ControlResponse {
        schema_version,
        command_id,
        control_revision,
        workflow_status,
        result,
    })
}

/// Decodes one complete canonical control response.
///
/// # Errors
///
/// Returns [`CodecError`] when the response is malformed, truncated,
/// non-canonical, or uses an unsupported schema/tag.
pub fn decode_control_response(bytes: &[u8]) -> Result<ControlResponse, CodecError> {
    let mut reader = Reader::new(bytes);
    if reader.take(8)? != b"BMRES001" {
        return Err(CodecError::InvalidMagic);
    }
    let response = decode_state_response(&mut reader)?;
    reader.finish()?;
    Ok(response)
}

#[must_use]
pub fn encode_control_response(response: &ControlResponse) -> Vec<u8> {
    let mut writer = Writer::default();
    writer.raw(b"BMRES001");
    encode_state_response(&mut writer, response);
    writer.into_bytes()
}

fn validate_command(command: &ControlCommand) -> Result<(), CodecError> {
    validate_string(&command.principal_id)?;
    validate_request(&command.request)
}

fn validate_request(request: &ControlCommandRequest) -> Result<(), CodecError> {
    match request {
        ControlCommandRequest::UpsertMember { member } => validate_member(member),
        ControlCommandRequest::SetMemberState {
            node_id,
            expected_credential_serial,
            ..
        } => {
            validate_string(node_id)?;
            validate_string(expected_credential_serial)
        }
        ControlCommandRequest::CreateWorkspace { name, .. }
        | ControlCommandRequest::RenameWorkspace { name, .. } => {
            validate_optional_string(name.as_deref())
        }
        ControlCommandRequest::PutWindow { window, .. } => validate_window(window),
        ControlCommandRequest::RemoveWindow { .. }
        | ControlCommandRequest::RemovePane { .. }
        | ControlCommandRequest::PruneDedup { .. } => Ok(()),
        ControlCommandRequest::PutPane { pane, .. } => validate_pane(pane),
        ControlCommandRequest::AssignExecution {
            assignment,
            launch_spec,
            ..
        } => {
            validate_assignment(assignment)?;
            if let Some(spec) = launch_spec {
                validate_optional_string(spec.program.as_deref())?;
                validate_optional_string(spec.cwd.as_deref())?;
                if spec.args.len() > MAX_LIST_ITEMS || spec.env.len() > MAX_LIST_ITEMS {
                    return Err(CodecError::LimitExceeded("launch spec items"));
                }
                for argument in &spec.args {
                    validate_string(argument)?;
                }
                for entry in &spec.env {
                    validate_string(&entry.key)?;
                    validate_string(&entry.value)?;
                }
            }
            Ok(())
        }
        ControlCommandRequest::SetPaneAvailability {
            assignment, reason, ..
        } => {
            validate_assignment(assignment)?;
            validate_optional_string(reason.as_deref())
        }
        ControlCommandRequest::CompleteWorkflow { response, .. } => validate_bytes(response),
    }
}

fn validate_window(window: &LogicalWindowRecord) -> Result<(), CodecError> {
    validate_optional_string(window.name.as_deref())?;
    validate_bytes(&window.layout)
}

fn validate_pane(pane: &LogicalPaneRecord) -> Result<(), CodecError> {
    validate_optional_string(pane.name.as_deref())?;
    validate_optional_string(pane.availability_reason.as_deref())?;
    validate_optional_string(pane.placement.explicit_node_id.as_deref())?;
    validate_labels(&pane.placement.required_labels)?;
    validate_labels(&pane.placement.preferred_labels)?;
    if let Some(assignment) = &pane.execution {
        validate_assignment(assignment)?;
    }
    Ok(())
}

fn validate_labels(labels: &[PlacementLabel]) -> Result<(), CodecError> {
    if labels.len() > MAX_LIST_ITEMS {
        return Err(CodecError::LimitExceeded("label list"));
    }
    for label in labels {
        validate_string(&label.key)?;
        validate_string(&label.value)?;
    }
    Ok(())
}

fn validate_assignment(assignment: &ExecutionAssignment) -> Result<(), CodecError> {
    validate_string(&assignment.node_id)
}

fn validate_member(member: &ClusterMember) -> Result<(), CodecError> {
    for value in [
        &member.cluster_id,
        &member.node_id,
        &member.public_key,
        &member.credential_serial,
        &member.credential_issuer_node_id,
        &member.credential_issuer_public_key,
        &member.credential_signature,
        &member.negotiated_protocol.local_plugin_version,
        &member.negotiated_protocol.remote_plugin_version,
    ] {
        validate_string(value)?;
    }
    validate_optional_string(member.endpoint.as_deref())?;
    if member.negotiated_protocol.features.len() > MAX_LIST_ITEMS {
        return Err(CodecError::LimitExceeded("feature list"));
    }
    for feature in &member.negotiated_protocol.features {
        validate_string(feature)?;
    }
    Ok(())
}

fn validate_optional_string(value: Option<&str>) -> Result<(), CodecError> {
    value.map_or(Ok(()), validate_string)
}

const fn validate_string(value: &str) -> Result<(), CodecError> {
    if value.len() > MAX_STRING_BYTES {
        Err(CodecError::LimitExceeded("string"))
    } else {
        Ok(())
    }
}

const fn validate_bytes(value: &[u8]) -> Result<(), CodecError> {
    if value.len() > MAX_BYTES {
        Err(CodecError::LimitExceeded("byte field"))
    } else {
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
fn encode_request(writer: &mut Writer, request: &ControlCommandRequest) {
    match request {
        ControlCommandRequest::UpsertMember { member } => {
            writer.u16(1);
            encode_member(writer, member);
        }
        ControlCommandRequest::SetMemberState {
            node_id,
            expected_credential_serial,
            state,
        } => {
            writer.u16(2);
            writer.string(node_id);
            writer.string(expected_credential_serial);
            encode_member_state(writer, *state);
        }
        ControlCommandRequest::CreateWorkspace { workspace_id, name } => {
            writer.u16(3);
            encode_workspace_id(writer, workspace_id);
            writer.optional_string(name.as_deref());
        }
        ControlCommandRequest::RenameWorkspace {
            workspace_id,
            expected_revision,
            name,
        } => {
            writer.u16(4);
            encode_workspace_id(writer, workspace_id);
            writer.u64(*expected_revision);
            writer.optional_string(name.as_deref());
        }
        ControlCommandRequest::PutWindow {
            window,
            expected_workspace_revision,
        } => {
            writer.u16(5);
            encode_window(writer, window);
            writer.u64(*expected_workspace_revision);
        }
        ControlCommandRequest::RemoveWindow {
            window_id,
            expected_workspace_revision,
        } => {
            writer.u16(6);
            writer.uuid(window_id.value);
            writer.u64(*expected_workspace_revision);
        }
        ControlCommandRequest::PutPane {
            pane,
            expected_workspace_revision,
        } => {
            writer.u16(7);
            encode_pane(writer, pane);
            writer.u64(*expected_workspace_revision);
        }
        ControlCommandRequest::RemovePane {
            pane_id,
            expected_revision,
            expected_generation,
        } => {
            writer.u16(8);
            writer.uuid(pane_id.value);
            writer.u64(*expected_revision);
            writer.optional_u64(*expected_generation);
        }
        ControlCommandRequest::AssignExecution {
            pane_id,
            expected_revision,
            expected_generation,
            assignment,
            launch_spec,
        } => {
            writer.u16(9);
            writer.uuid(pane_id.value);
            writer.u64(*expected_revision);
            writer.u64(*expected_generation);
            encode_assignment(writer, assignment);
            encode_launch_spec(writer, launch_spec.as_ref());
        }
        ControlCommandRequest::SetPaneAvailability {
            pane_id,
            expected_revision,
            assignment,
            availability,
            reason,
        } => {
            writer.u16(10);
            writer.uuid(pane_id.value);
            writer.u64(*expected_revision);
            encode_assignment(writer, assignment);
            encode_availability(writer, *availability);
            writer.optional_string(reason.as_deref());
        }
        ControlCommandRequest::CompleteWorkflow {
            original_command_id,
            response,
        } => {
            writer.u16(11);
            writer.uuid(original_command_id.value);
            writer.bytes(response);
        }
        ControlCommandRequest::PruneDedup {
            completed_before_unix_ms,
        } => {
            writer.u16(12);
            writer.u64(*completed_before_unix_ms);
        }
    }
}

fn decode_request(reader: &mut Reader<'_>) -> Result<ControlCommandRequest, CodecError> {
    Ok(match reader.u16()? {
        1 => ControlCommandRequest::UpsertMember {
            member: decode_member(reader)?,
        },
        2 => ControlCommandRequest::SetMemberState {
            node_id: reader.string()?,
            expected_credential_serial: reader.string()?,
            state: decode_member_state(reader)?,
        },
        3 => ControlCommandRequest::CreateWorkspace {
            workspace_id: decode_workspace_id(reader)?,
            name: reader.optional_string()?,
        },
        4 => ControlCommandRequest::RenameWorkspace {
            workspace_id: decode_workspace_id(reader)?,
            expected_revision: reader.u64()?,
            name: reader.optional_string()?,
        },
        5 => ControlCommandRequest::PutWindow {
            window: decode_window(reader)?,
            expected_workspace_revision: reader.u64()?,
        },
        6 => ControlCommandRequest::RemoveWindow {
            window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
                value: reader.uuid()?,
            },
            expected_workspace_revision: reader.u64()?,
        },
        7 => ControlCommandRequest::PutPane {
            pane: decode_pane(reader)?,
            expected_workspace_revision: reader.u64()?,
        },
        8 => ControlCommandRequest::RemovePane {
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: reader.uuid()?,
            },
            expected_revision: reader.u64()?,
            expected_generation: reader.optional_u64()?,
        },
        9 => ControlCommandRequest::AssignExecution {
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: reader.uuid()?,
            },
            expected_revision: reader.u64()?,
            expected_generation: reader.u64()?,
            assignment: decode_assignment(reader)?,
            launch_spec: decode_launch_spec(reader)?,
        },
        10 => ControlCommandRequest::SetPaneAvailability {
            pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
                value: reader.uuid()?,
            },
            expected_revision: reader.u64()?,
            assignment: decode_assignment(reader)?,
            availability: decode_availability(reader)?,
            reason: reader.optional_string()?,
        },
        11 => ControlCommandRequest::CompleteWorkflow {
            original_command_id: CommandId {
                value: reader.uuid()?,
            },
            response: reader.bytes()?,
        },
        12 => ControlCommandRequest::PruneDedup {
            completed_before_unix_ms: reader.u64()?,
        },
        tag => {
            return Err(CodecError::InvalidTag {
                type_name: "control command",
                tag,
            });
        }
    })
}

fn encode_workspace_id(writer: &mut Writer, id: &WorkspaceId) {
    writer.uuid(id.value);
}

fn decode_workspace_id(reader: &mut Reader<'_>) -> Result<WorkspaceId, CodecError> {
    Ok(WorkspaceId {
        value: reader.uuid()?,
    })
}

fn encode_window(writer: &mut Writer, window: &LogicalWindowRecord) {
    writer.uuid(window.window_id.value);
    encode_workspace_id(writer, &window.workspace_id);
    writer.optional_string(window.name.as_deref());
    writer.u32(window.layout_schema_version);
    writer.bytes(&window.layout);
    writer.u64(window.revision);
}

fn decode_window(reader: &mut Reader<'_>) -> Result<LogicalWindowRecord, CodecError> {
    Ok(LogicalWindowRecord {
        window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
            value: reader.uuid()?,
        },
        workspace_id: decode_workspace_id(reader)?,
        name: reader.optional_string()?,
        layout_schema_version: reader.u32()?,
        layout: reader.bytes()?,
        revision: reader.u64()?,
    })
}

fn encode_pane(writer: &mut Writer, pane: &LogicalPaneRecord) {
    writer.uuid(pane.pane_id.value);
    writer.uuid(pane.workspace_id.value);
    writer.uuid(pane.window_id.value);
    writer.optional_string(pane.name.as_deref());
    encode_restart_policy(writer, pane.restart_policy);
    encode_placement(writer, &pane.placement);
    encode_availability(writer, pane.availability);
    writer.optional_string(pane.availability_reason.as_deref());
    match &pane.execution {
        Some(assignment) => {
            writer.bool(true);
            encode_assignment(writer, assignment);
        }
        None => writer.bool(false),
    }
    writer.u64(pane.revision);
}

fn decode_pane(reader: &mut Reader<'_>) -> Result<LogicalPaneRecord, CodecError> {
    Ok(LogicalPaneRecord {
        pane_id: bmux_cluster_plugin_api::cluster_types::LogicalPaneId {
            value: reader.uuid()?,
        },
        workspace_id: decode_workspace_id(reader)?,
        window_id: bmux_cluster_plugin_api::cluster_types::LogicalWindowId {
            value: reader.uuid()?,
        },
        name: reader.optional_string()?,
        restart_policy: decode_restart_policy(reader)?,
        placement: decode_placement(reader)?,
        availability: decode_availability(reader)?,
        availability_reason: reader.optional_string()?,
        execution: if reader.bool()? {
            Some(decode_assignment(reader)?)
        } else {
            None
        },
        revision: reader.u64()?,
    })
}

fn encode_placement(writer: &mut Writer, placement: &PlacementIntent) {
    writer.optional_string(placement.explicit_node_id.as_deref());
    encode_labels(writer, &placement.required_labels);
    encode_labels(writer, &placement.preferred_labels);
}

fn decode_placement(reader: &mut Reader<'_>) -> Result<PlacementIntent, CodecError> {
    Ok(PlacementIntent {
        explicit_node_id: reader.optional_string()?,
        required_labels: decode_labels(reader)?,
        preferred_labels: decode_labels(reader)?,
    })
}

fn encode_labels(writer: &mut Writer, labels: &[PlacementLabel]) {
    writer.len(labels.len());
    for label in labels {
        writer.string(&label.key);
        writer.string(&label.value);
    }
}

fn decode_labels(reader: &mut Reader<'_>) -> Result<Vec<PlacementLabel>, CodecError> {
    let len = reader.len("label list", MAX_LIST_ITEMS)?;
    (0..len)
        .map(|_| {
            Ok(PlacementLabel {
                key: reader.string()?,
                value: reader.string()?,
            })
        })
        .collect()
}

fn encode_assignment(writer: &mut Writer, assignment: &ExecutionAssignment) {
    writer.string(&assignment.node_id);
    writer.u64(assignment.generation);
    writer.uuid(assignment.execution_id.value);
}

fn decode_assignment(reader: &mut Reader<'_>) -> Result<ExecutionAssignment, CodecError> {
    Ok(ExecutionAssignment {
        node_id: reader.string()?,
        generation: reader.u64()?,
        execution_id: bmux_cluster_plugin_api::cluster_types::ExecutionId {
            value: reader.uuid()?,
        },
    })
}

fn encode_member(writer: &mut Writer, member: &ClusterMember) {
    writer.string(&member.cluster_id);
    writer.string(&member.node_id);
    writer.string(&member.public_key);
    writer.optional_string(member.endpoint.as_deref());
    encode_capabilities(writer, &member.capabilities);
    writer.string(&member.credential_serial);
    writer.string(&member.credential_issuer_node_id);
    writer.string(&member.credential_issuer_public_key);
    writer.u64(member.credential_issued_at_unix_ms);
    writer.u64(member.credential_expires_at_unix_ms);
    writer.string(&member.credential_signature);
    encode_protocol(writer, &member.negotiated_protocol);
    writer.u64(member.joined_at_unix_ms);
    writer.u64(member.updated_at_unix_ms);
    encode_member_state(writer, member.state);
}

fn decode_member(reader: &mut Reader<'_>) -> Result<ClusterMember, CodecError> {
    Ok(ClusterMember {
        cluster_id: reader.string()?,
        node_id: reader.string()?,
        public_key: reader.string()?,
        endpoint: reader.optional_string()?,
        capabilities: decode_capabilities(reader)?,
        credential_serial: reader.string()?,
        credential_issuer_node_id: reader.string()?,
        credential_issuer_public_key: reader.string()?,
        credential_issued_at_unix_ms: reader.u64()?,
        credential_expires_at_unix_ms: reader.u64()?,
        credential_signature: reader.string()?,
        negotiated_protocol: decode_protocol(reader)?,
        joined_at_unix_ms: reader.u64()?,
        updated_at_unix_ms: reader.u64()?,
        state: decode_member_state(reader)?,
    })
}

fn encode_capabilities(writer: &mut Writer, capabilities: &ClusterNodeCapabilities) {
    writer.u16(match capabilities.consensus_role {
        ClusterConsensusRole::Voter => 1,
        ClusterConsensusRole::ObserverEdge => 2,
    });
    writer.bool(capabilities.worker);
    writer.bool(capabilities.ingress);
}

fn decode_capabilities(reader: &mut Reader<'_>) -> Result<ClusterNodeCapabilities, CodecError> {
    let consensus_role = match reader.u16()? {
        1 => ClusterConsensusRole::Voter,
        2 => ClusterConsensusRole::ObserverEdge,
        tag => {
            return Err(CodecError::InvalidTag {
                type_name: "consensus role",
                tag,
            });
        }
    };
    Ok(ClusterNodeCapabilities {
        consensus_role,
        worker: reader.bool()?,
        ingress: reader.bool()?,
    })
}

fn encode_protocol(writer: &mut Writer, protocol: &ClusterNegotiatedProtocol) {
    writer.u16(protocol.wire_epoch);
    writer.u32(protocol.peer_revision);
    writer.u32(protocol.schema_version);
    writer.string(&protocol.local_plugin_version);
    writer.string(&protocol.remote_plugin_version);
    writer.len(protocol.features.len());
    for feature in &protocol.features {
        writer.string(feature);
    }
}

fn decode_protocol(reader: &mut Reader<'_>) -> Result<ClusterNegotiatedProtocol, CodecError> {
    let wire_epoch = reader.u16()?;
    let peer_revision = reader.u32()?;
    let schema_version = reader.u32()?;
    let local_plugin_version = reader.string()?;
    let remote_plugin_version = reader.string()?;
    let len = reader.len("feature list", MAX_LIST_ITEMS)?;
    let features = (0..len)
        .map(|_| reader.string())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ClusterNegotiatedProtocol {
        wire_epoch,
        peer_revision,
        schema_version,
        local_plugin_version,
        remote_plugin_version,
        features,
    })
}

fn encode_member_state(writer: &mut Writer, state: ClusterMemberState) {
    writer.u16(match state {
        ClusterMemberState::Active => 1,
        ClusterMemberState::Revoked => 2,
        ClusterMemberState::Left => 3,
    });
}

fn decode_member_state(reader: &mut Reader<'_>) -> Result<ClusterMemberState, CodecError> {
    match reader.u16()? {
        1 => Ok(ClusterMemberState::Active),
        2 => Ok(ClusterMemberState::Revoked),
        3 => Ok(ClusterMemberState::Left),
        tag => Err(CodecError::InvalidTag {
            type_name: "member state",
            tag,
        }),
    }
}

fn encode_launch_spec(
    writer: &mut Writer,
    spec: Option<&bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec>,
) {
    let Some(spec) = spec else {
        writer.bool(false);
        return;
    };
    writer.bool(true);
    writer.optional_string(spec.program.as_deref());
    writer.u32(u32::try_from(spec.args.len()).expect("validated launch argument count"));
    for argument in &spec.args {
        writer.string(argument);
    }
    writer.optional_string(spec.cwd.as_deref());
    encode_labels(writer, &spec.env);
    writer.u16(spec.cols);
    writer.u16(spec.rows);
}

fn decode_launch_spec(
    reader: &mut Reader<'_>,
) -> Result<Option<bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec>, CodecError> {
    if !reader.bool()? {
        return Ok(None);
    }
    let program = reader.optional_string()?;
    let count = usize::try_from(reader.u32()?)
        .map_err(|_| CodecError::LimitExceeded("launch argument count"))?;
    if count > MAX_LIST_ITEMS {
        return Err(CodecError::LimitExceeded("launch argument count"));
    }
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        args.push(reader.string()?);
    }
    Ok(Some(
        bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec {
            program,
            args,
            cwd: reader.optional_string()?,
            env: decode_labels(reader)?,
            cols: reader.u16()?,
            rows: reader.u16()?,
        },
    ))
}

fn encode_restart_policy(writer: &mut Writer, policy: PaneRestartPolicy) {
    writer.u16(match policy {
        PaneRestartPolicy::Manual => 1,
        PaneRestartPolicy::Never => 2,
        PaneRestartPolicy::OnWorkerLoss => 3,
    });
}

fn decode_restart_policy(reader: &mut Reader<'_>) -> Result<PaneRestartPolicy, CodecError> {
    match reader.u16()? {
        1 => Ok(PaneRestartPolicy::Manual),
        2 => Ok(PaneRestartPolicy::Never),
        3 => Ok(PaneRestartPolicy::OnWorkerLoss),
        tag => Err(CodecError::InvalidTag {
            type_name: "restart policy",
            tag,
        }),
    }
}

fn encode_availability(writer: &mut Writer, availability: PaneAvailability) {
    writer.u16(match availability {
        PaneAvailability::Pending => 1,
        PaneAvailability::Ready => 2,
        PaneAvailability::Suspect => 3,
        PaneAvailability::Unavailable => 4,
        PaneAvailability::Reconciling => 5,
        PaneAvailability::Replacing => 6,
        PaneAvailability::Exited => 7,
        PaneAvailability::Failed => 8,
        PaneAvailability::Quarantined => 9,
    });
}

fn decode_availability(reader: &mut Reader<'_>) -> Result<PaneAvailability, CodecError> {
    match reader.u16()? {
        1 => Ok(PaneAvailability::Pending),
        2 => Ok(PaneAvailability::Ready),
        3 => Ok(PaneAvailability::Suspect),
        4 => Ok(PaneAvailability::Unavailable),
        5 => Ok(PaneAvailability::Reconciling),
        6 => Ok(PaneAvailability::Replacing),
        7 => Ok(PaneAvailability::Exited),
        8 => Ok(PaneAvailability::Failed),
        9 => Ok(PaneAvailability::Quarantined),
        tag => Err(CodecError::InvalidTag {
            type_name: "pane availability",
            tag,
        }),
    }
}

fn encode_error(writer: &mut Writer, error: &ControlCommandError) {
    match error {
        ControlCommandError::CommandIdConflict => writer.u16(1),
        ControlCommandError::NotFound { resource, id } => {
            writer.u16(2);
            encode_resource_kind(writer, *resource);
            writer.string(id);
        }
        ControlCommandError::AlreadyExists { resource, id } => {
            writer.u16(3);
            encode_resource_kind(writer, *resource);
            writer.string(id);
        }
        ControlCommandError::RevisionConflict { expected, current } => {
            writer.u16(4);
            writer.u64(*expected);
            writer.u64(*current);
        }
        ControlCommandError::GenerationConflict { expected, current } => {
            writer.u16(5);
            writer.u64(*expected);
            writer.u64(*current);
        }
        ControlCommandError::InvalidReference { resource, id } => {
            writer.u16(6);
            encode_resource_kind(writer, *resource);
            writer.string(id);
        }
        ControlCommandError::InvalidTransition { reason } => {
            writer.u16(7);
            writer.string(reason);
        }
        ControlCommandError::MemberInactive { node_id } => {
            writer.u16(8);
            writer.string(node_id);
        }
        ControlCommandError::IncompatibleSchema {
            supported,
            received,
        } => {
            writer.u16(9);
            writer.u16(*supported);
            writer.u16(*received);
        }
        ControlCommandError::QuorumRequired => writer.u16(10),
        ControlCommandError::NotLeader { leader_node_id } => {
            writer.u16(11);
            writer.optional_string(leader_node_id.as_deref());
        }
    }
}

fn decode_error(reader: &mut Reader<'_>) -> Result<ControlCommandError, CodecError> {
    Ok(match reader.u16()? {
        1 => ControlCommandError::CommandIdConflict,
        2 => ControlCommandError::NotFound {
            resource: decode_resource_kind(reader)?,
            id: reader.string()?,
        },
        3 => ControlCommandError::AlreadyExists {
            resource: decode_resource_kind(reader)?,
            id: reader.string()?,
        },
        4 => ControlCommandError::RevisionConflict {
            expected: reader.u64()?,
            current: reader.u64()?,
        },
        5 => ControlCommandError::GenerationConflict {
            expected: reader.u64()?,
            current: reader.u64()?,
        },
        6 => ControlCommandError::InvalidReference {
            resource: decode_resource_kind(reader)?,
            id: reader.string()?,
        },
        7 => ControlCommandError::InvalidTransition {
            reason: reader.string()?,
        },
        8 => ControlCommandError::MemberInactive {
            node_id: reader.string()?,
        },
        9 => ControlCommandError::IncompatibleSchema {
            supported: reader.u16()?,
            received: reader.u16()?,
        },
        10 => ControlCommandError::QuorumRequired,
        11 => ControlCommandError::NotLeader {
            leader_node_id: reader.optional_string()?,
        },
        tag => {
            return Err(CodecError::InvalidTag {
                type_name: "control error",
                tag,
            });
        }
    })
}

fn encode_resource_kind(writer: &mut Writer, kind: ControlResourceKind) {
    writer.u16(match kind {
        ControlResourceKind::Member => 1,
        ControlResourceKind::Workspace => 2,
        ControlResourceKind::Window => 3,
        ControlResourceKind::Pane => 4,
        ControlResourceKind::Execution => 5,
        ControlResourceKind::Workflow => 6,
    });
}

fn decode_resource_kind(reader: &mut Reader<'_>) -> Result<ControlResourceKind, CodecError> {
    match reader.u16()? {
        1 => Ok(ControlResourceKind::Member),
        2 => Ok(ControlResourceKind::Workspace),
        3 => Ok(ControlResourceKind::Window),
        4 => Ok(ControlResourceKind::Pane),
        5 => Ok(ControlResourceKind::Execution),
        6 => Ok(ControlResourceKind::Workflow),
        tag => Err(CodecError::InvalidTag {
            type_name: "resource kind",
            tag,
        }),
    }
}

#[derive(Default)]
pub(crate) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(crate) fn encode_state_workspace(&mut self, workspace: &WorkspaceRecord) {
        encode_workspace_id(self, &workspace.workspace_id);
        self.optional_string(workspace.name.as_deref());
        self.u64(workspace.revision);
    }

    pub(crate) fn encode_state_member(&mut self, member: &ClusterMember) {
        encode_member(self, member);
    }

    pub(crate) fn encode_state_window(&mut self, window: &LogicalWindowRecord) {
        encode_window(self, window);
    }

    pub(crate) fn encode_state_pane(&mut self, pane: &LogicalPaneRecord) {
        encode_pane(self, pane);
    }

    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(crate) fn raw(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    pub(crate) fn bool(&mut self, value: bool) {
        self.bytes.push(u8::from(value));
    }

    pub(crate) fn u16(&mut self, value: u16) {
        self.raw(&value.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, value: u32) {
        self.raw(&value.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, value: u64) {
        self.raw(&value.to_be_bytes());
    }

    pub(crate) fn len(&mut self, value: usize) {
        self.u32(u32::try_from(value).expect("validated canonical length must fit u32"));
    }

    pub(crate) fn string(&mut self, value: &str) {
        assert!(
            value.len() <= MAX_STRING_BYTES,
            "canonical string exceeds limit"
        );
        self.len(value.len());
        self.raw(value.as_bytes());
    }

    pub(crate) fn optional_string(&mut self, value: Option<&str>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.string(value);
            }
            None => self.bool(false),
        }
    }

    pub(crate) fn optional_u64(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.bool(true);
                self.u64(value);
            }
            None => self.bool(false),
        }
    }

    pub(crate) fn bytes(&mut self, value: &[u8]) {
        assert!(
            value.len() <= MAX_BYTES,
            "canonical byte field exceeds limit"
        );
        self.len(value.len());
        self.raw(value);
    }

    pub(crate) fn uuid(&mut self, value: uuid::Uuid) {
        self.raw(value.as_bytes());
    }
}

pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    pub(crate) fn decode_state_workspace(&mut self) -> Result<WorkspaceRecord, CodecError> {
        Ok(WorkspaceRecord {
            workspace_id: decode_workspace_id(self)?,
            name: self.optional_string()?,
            revision: self.u64()?,
        })
    }

    pub(crate) fn decode_state_member(&mut self) -> Result<ClusterMember, CodecError> {
        decode_member(self)
    }

    pub(crate) fn decode_state_window(&mut self) -> Result<LogicalWindowRecord, CodecError> {
        decode_window(self)
    }

    pub(crate) fn decode_state_pane(&mut self) -> Result<LogicalPaneRecord, CodecError> {
        decode_pane(self)
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], CodecError> {
        let end = self.offset.checked_add(len).ok_or(CodecError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CodecError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    pub(crate) const fn finish(self) -> Result<(), CodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(CodecError::TrailingBytes)
        }
    }

    pub(crate) fn bool(&mut self) -> Result<bool, CodecError> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(CodecError::InvalidBoolean(value)),
        }
    }

    pub(crate) fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("exact length"),
        ))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("exact length"),
        ))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn len(&mut self, name: &'static str, max: usize) -> Result<usize, CodecError> {
        let len = usize::try_from(self.u32()?).expect("u32 must fit usize");
        if len > max {
            return Err(CodecError::LimitExceeded(name));
        }
        Ok(len)
    }

    pub(crate) fn string(&mut self) -> Result<String, CodecError> {
        let len = self.len("string", MAX_STRING_BYTES)?;
        std::str::from_utf8(self.take(len)?)
            .map(ToString::to_string)
            .map_err(|_| CodecError::InvalidUtf8)
    }

    pub(crate) fn optional_string(&mut self) -> Result<Option<String>, CodecError> {
        if self.bool()? {
            Ok(Some(self.string()?))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn optional_u64(&mut self) -> Result<Option<u64>, CodecError> {
        if self.bool()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>, CodecError> {
        let len = self.len("byte field", MAX_BYTES)?;
        Ok(self.take(len)?.to_vec())
    }

    pub(crate) fn uuid(&mut self) -> Result<uuid::Uuid, CodecError> {
        Ok(uuid::Uuid::from_bytes(
            self.take(16)?.try_into().expect("exact length"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::cluster_types::{
        ControlCommandRequest, ExecutionId, LogicalPaneId, LogicalWindowId,
    };

    fn id(value: u128) -> uuid::Uuid {
        uuid::Uuid::from_u128(value)
    }

    fn command(request: ControlCommandRequest) -> ControlCommand {
        ControlCommand {
            schema_version: CONTROL_SCHEMA_VERSION,
            principal_id: "principal:test".to_string(),
            command_id: CommandId { value: id(1) },
            issued_at_unix_ms: 42,
            request,
        }
    }

    fn assignment() -> ExecutionAssignment {
        ExecutionAssignment {
            node_id: "node:worker".to_string(),
            generation: 1,
            execution_id: ExecutionId { value: id(5) },
        }
    }

    fn pane() -> LogicalPaneRecord {
        LogicalPaneRecord {
            pane_id: LogicalPaneId { value: id(4) },
            workspace_id: WorkspaceId { value: id(2) },
            window_id: LogicalWindowId { value: id(3) },
            name: Some("shell".to_string()),
            restart_policy: PaneRestartPolicy::Manual,
            placement: PlacementIntent {
                explicit_node_id: Some("node:worker".to_string()),
                required_labels: vec![PlacementLabel {
                    key: "os".to_string(),
                    value: "linux".to_string(),
                }],
                preferred_labels: Vec::new(),
            },
            availability: PaneAvailability::Ready,
            availability_reason: None,
            execution: Some(assignment()),
            revision: 1,
        }
    }

    #[test]
    fn feature_activation_codec_is_distinct_canonical_and_bounded() {
        let command = FeatureActivationCommand {
            principal_id: "principal:test".to_string(),
            command_id: CommandId { value: id(77) },
            issued_at_unix_ms: 42,
            expected_control_revision: 9,
            read_schema_floor: 2,
            write_schema_floor: 2,
            feature: "atomic-layout-mutation-v2".to_string(),
        };
        let encoded = encode_feature_activation(&command);
        assert!(is_feature_activation(&encoded));
        assert!(!encoded.starts_with(CODEC_MAGIC));
        assert_eq!(decode_feature_activation(&encoded).unwrap(), command);
        assert_eq!(encode_feature_activation(&command), encoded);
        assert!(decode_control_command(&encoded).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_initial_command_round_trips_canonically() {
        let member = ClusterMember {
            cluster_id: "cluster:test".to_string(),
            node_id: "node:member".to_string(),
            public_key: "public".to_string(),
            endpoint: Some("peer".to_string()),
            capabilities: ClusterNodeCapabilities {
                consensus_role: ClusterConsensusRole::Voter,
                worker: true,
                ingress: true,
            },
            credential_serial: "serial".to_string(),
            credential_issuer_node_id: "node:issuer".to_string(),
            credential_issuer_public_key: "issuer-public".to_string(),
            credential_issued_at_unix_ms: 1,
            credential_expires_at_unix_ms: 2,
            credential_signature: "signature".to_string(),
            negotiated_protocol: ClusterNegotiatedProtocol {
                wire_epoch: 3,
                peer_revision: 1,
                schema_version: 1,
                local_plugin_version: "1".to_string(),
                remote_plugin_version: "1".to_string(),
                features: vec!["a".to_string(), "b".to_string()],
            },
            joined_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            state: ClusterMemberState::Active,
        };
        let window = LogicalWindowRecord {
            window_id: LogicalWindowId { value: id(3) },
            workspace_id: WorkspaceId { value: id(2) },
            name: Some("main".to_string()),
            layout_schema_version: 1,
            layout: vec![1, 2, 3],
            revision: 1,
        };
        let requests = vec![
            ControlCommandRequest::UpsertMember { member },
            ControlCommandRequest::SetMemberState {
                node_id: "node:member".to_string(),
                expected_credential_serial: "serial".to_string(),
                state: ClusterMemberState::Revoked,
            },
            ControlCommandRequest::CreateWorkspace {
                workspace_id: WorkspaceId { value: id(2) },
                name: Some("workspace".to_string()),
            },
            ControlCommandRequest::RenameWorkspace {
                workspace_id: WorkspaceId { value: id(2) },
                expected_revision: 1,
                name: None,
            },
            ControlCommandRequest::PutWindow {
                window,
                expected_workspace_revision: 1,
            },
            ControlCommandRequest::RemoveWindow {
                window_id: LogicalWindowId { value: id(3) },
                expected_workspace_revision: 1,
            },
            ControlCommandRequest::PutPane {
                pane: pane(),
                expected_workspace_revision: 1,
            },
            ControlCommandRequest::RemovePane {
                pane_id: LogicalPaneId { value: id(4) },
                expected_revision: 1,
                expected_generation: Some(1),
            },
            ControlCommandRequest::AssignExecution {
                pane_id: LogicalPaneId { value: id(4) },
                expected_revision: 1,
                expected_generation: 0,
                assignment: assignment(),
                launch_spec: Some(bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec {
                    program: Some("sh".to_string()),
                    args: vec!["-lc".to_string(), "echo ok".to_string()],
                    cwd: Some("/tmp".to_string()),
                    env: vec![PlacementLabel {
                        key: "TERM".to_string(),
                        value: "xterm-256color".to_string(),
                    }],
                    cols: 80,
                    rows: 24,
                }),
            },
            ControlCommandRequest::SetPaneAvailability {
                pane_id: LogicalPaneId { value: id(4) },
                expected_revision: 1,
                assignment: assignment(),
                availability: PaneAvailability::Unavailable,
                reason: Some("network".to_string()),
            },
            ControlCommandRequest::CompleteWorkflow {
                original_command_id: CommandId { value: id(9) },
                response: vec![4, 5],
            },
            ControlCommandRequest::PruneDedup {
                completed_before_unix_ms: 100,
            },
        ];
        for request in requests {
            let original = command(request);
            let encoded = encode_control_command(&original);
            assert_eq!(decode_control_command(&encoded).unwrap(), original);
            assert_eq!(encode_control_command(&original), encoded);
        }
    }

    #[test]
    fn golden_fixture_and_fingerprint_are_stable() {
        let command = command(ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId { value: id(2) },
            name: Some("ops".to_string()),
        });
        let encoded = encode_control_command(&command);
        assert_eq!(
            hex(&encoded),
            "424d434d4430303100010000000e7072696e636970616c3a7465737400000000000000000000000000000001000000000000002a00030000000000000000000000000000000201000000036f7073"
        );
        assert_eq!(
            hex(&request_fingerprint(&command)),
            "88a2e0542d6a4a3d1034458f42992671a7645a4b08ab4411aa59d0e363b5b3e3"
        );
    }

    #[test]
    fn malformed_noncanonical_input_is_rejected() {
        let valid = encode_control_command(&command(ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId { value: id(2) },
            name: None,
        }));
        assert_eq!(
            decode_control_command(&valid[..valid.len() - 1]),
            Err(CodecError::Truncated)
        );
        let mut trailing = valid.clone();
        trailing.push(0);
        assert_eq!(
            decode_control_command(&trailing),
            Err(CodecError::TrailingBytes)
        );
        let mut invalid_bool = valid.clone();
        let last = invalid_bool.len() - 1;
        invalid_bool[last] = 2;
        assert_eq!(
            decode_control_command(&invalid_bool),
            Err(CodecError::InvalidBoolean(2))
        );
        let mut future = valid;
        future[8..10].copy_from_slice(&2_u16.to_be_bytes());
        assert_eq!(
            decode_control_command(&future),
            Err(CodecError::UnsupportedSchema(2))
        );
    }

    #[test]
    fn length_limits_fail_before_encoding() {
        let oversized = command(ControlCommandRequest::CreateWorkspace {
            workspace_id: WorkspaceId { value: id(2) },
            name: Some("x".repeat(MAX_STRING_BYTES + 1)),
        });
        assert_eq!(
            validate_command(&oversized),
            Err(CodecError::LimitExceeded("string"))
        );
    }

    #[test]
    fn response_error_tags_are_stable() {
        let errors = [
            ControlCommandError::CommandIdConflict,
            ControlCommandError::NotFound {
                resource: ControlResourceKind::Pane,
                id: "pane".to_string(),
            },
            ControlCommandError::AlreadyExists {
                resource: ControlResourceKind::Workspace,
                id: "workspace".to_string(),
            },
            ControlCommandError::RevisionConflict {
                expected: 1,
                current: 2,
            },
            ControlCommandError::GenerationConflict {
                expected: 2,
                current: 3,
            },
            ControlCommandError::InvalidReference {
                resource: ControlResourceKind::Window,
                id: "window".to_string(),
            },
            ControlCommandError::InvalidTransition {
                reason: "bad".to_string(),
            },
            ControlCommandError::MemberInactive {
                node_id: "node".to_string(),
            },
            ControlCommandError::IncompatibleSchema {
                supported: 1,
                received: 2,
            },
            ControlCommandError::QuorumRequired,
            ControlCommandError::NotLeader {
                leader_node_id: Some("node:leader".to_string()),
            },
        ];
        for (index, error) in errors.into_iter().enumerate() {
            let response = ControlResponse {
                schema_version: 1,
                command_id: CommandId { value: id(1) },
                control_revision: 7,
                workflow_status: ControlWorkflowStatus::Complete,
                result: ControlCommandResult::Rejected { error },
            };
            let encoded = encode_control_response(&response);
            assert_eq!(
                u16::from_be_bytes(encoded[38..40].try_into().unwrap()),
                u16::try_from(index + 1).unwrap()
            );
        }
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;

        bytes.iter().fold(
            String::with_capacity(bytes.len() * 2),
            |mut output, byte| {
                write!(output, "{byte:02x}").expect("writing to String cannot fail");
                output
            },
        )
    }
}
