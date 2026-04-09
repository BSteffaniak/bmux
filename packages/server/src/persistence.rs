use bmux_config::ConfigPaths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

const SNAPSHOT_VERSION_V5: u32 = 5;
const MAX_SNAPSHOT_COMMAND_LEN: usize = 16 * 1024;
const MAX_SNAPSHOT_CWD_LEN: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotV4 {
    pub sessions: Vec<SessionSnapshotV3>,
    pub follows: Vec<FollowEdgeSnapshotV2>,
    pub selected_sessions: Vec<ClientSelectedSessionSnapshotV2>,
    pub contexts: Vec<ContextSnapshotV1>,
    pub context_session_bindings: Vec<ContextSessionBindingSnapshotV1>,
    pub selected_contexts: Vec<ClientSelectedContextSnapshotV1>,
    pub mru_contexts: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshotV1 {
    pub id: Uuid,
    pub name: Option<String>,
    #[serde(default)]
    pub attributes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSessionBindingSnapshotV1 {
    pub context_id: Uuid,
    pub session_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSelectedContextSnapshotV1 {
    pub client_id: Uuid,
    pub context_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshotV3 {
    pub id: Uuid,
    pub name: Option<String>,
    pub panes: Vec<PaneSnapshotV2>,
    pub focused_pane_id: Option<Uuid>,
    #[serde(default)]
    pub layout_root: Option<PaneLayoutNodeSnapshotV2>,
    #[serde(default)]
    pub floating_surfaces: Vec<FloatingSurfaceSnapshotV3>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FloatingSurfaceSnapshotV3 {
    pub id: Uuid,
    pub pane_id: Uuid,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub z: i32,
    pub visible: bool,
    pub opaque: bool,
    pub accepts_input: bool,
    pub cursor_owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshotV2 {
    pub id: Uuid,
    pub name: Option<String>,
    pub shell: String,
    #[serde(default)]
    pub process_group_id: Option<i32>,
    #[serde(default)]
    pub active_command: Option<String>,
    #[serde(default)]
    pub active_command_source: Option<crate::PaneCommandSource>,
    #[serde(default)]
    pub last_known_cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneLayoutNodeSnapshotV2 {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneSplitDirectionSnapshotV2,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirectionSnapshotV2 {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowEdgeSnapshotV2 {
    pub follower_client_id: Uuid,
    pub leader_client_id: Uuid,
    pub global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSelectedSessionSnapshotV2 {
    pub client_id: Uuid,
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SnapshotEnvelopeV4 {
    version: u32,
    checksum: u64,
    snapshot: SnapshotV4,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("snapshot io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("snapshot encode error: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("unsupported snapshot version: {0}")]
    UnsupportedVersion(u32),
    #[error("invalid snapshot: {0}")]
    Validation(String),
}

#[derive(Debug, Clone)]
pub struct SnapshotManager {
    path: PathBuf,
}

impl SnapshotManager {
    pub(crate) fn from_paths(paths: &ConfigPaths) -> Self {
        Self {
            path: paths
                .data_dir
                .join("runtime")
                .join("server-snapshot-v2.json"),
        }
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn encode_snapshot(snapshot: &SnapshotV4) -> Result<Vec<u8>, SnapshotError> {
        validate_snapshot_v4(snapshot)?;
        let checksum = snapshot_checksum_v4(snapshot).map_err(SnapshotError::Encode)?;
        let envelope = SnapshotEnvelopeV4 {
            version: SNAPSHOT_VERSION_V5,
            checksum,
            snapshot: snapshot.clone(),
        };
        serde_json::to_vec_pretty(&envelope).map_err(SnapshotError::Encode)
    }

    pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotV4, SnapshotError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| SnapshotError::Validation("snapshot missing version".to_string()))?;

        #[allow(clippy::cast_possible_truncation)]
        match version as u32 {
            SNAPSHOT_VERSION_V5 => {
                let envelope: SnapshotEnvelopeV4 = serde_json::from_value(value)?;
                let expected_checksum =
                    snapshot_checksum_v4(&envelope.snapshot).map_err(SnapshotError::Encode)?;
                if expected_checksum != envelope.checksum {
                    return Err(SnapshotError::Validation(
                        "snapshot checksum mismatch".to_string(),
                    ));
                }
                let snapshot = normalize_snapshot_v4_numbering(envelope.snapshot);
                validate_snapshot_v4(&snapshot)?;
                Ok(snapshot)
            }
            other => Err(SnapshotError::UnsupportedVersion(other)),
        }
    }

    pub(crate) fn write_snapshot(&self, snapshot: &SnapshotV4) -> Result<(), SnapshotError> {
        let encoded = Self::encode_snapshot(snapshot)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut temp_path = self.path.clone();
        let temp_name = self.path.file_name().map_or_else(
            || "server-snapshot.tmp".to_string(),
            |name| format!("{}.tmp", name.to_string_lossy()),
        );
        temp_path.set_file_name(temp_name);

        let mut temp_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)?;
        temp_file.write_all(&encoded)?;
        temp_file.sync_all()?;
        std::fs::rename(&temp_path, &self.path)?;
        if let Some(parent) = self.path.parent()
            && let Ok(parent_dir) = std::fs::File::open(parent)
        {
            let _ = parent_dir.sync_all();
        }
        Ok(())
    }

    pub(crate) fn read_snapshot(&self) -> Result<SnapshotV4, SnapshotError> {
        let bytes = std::fs::read(&self.path)?;
        Self::decode_snapshot(&bytes)
    }

    pub(crate) fn cleanup_temp_file(&self) -> Result<(), SnapshotError> {
        let mut temp_path = self.path.clone();
        let temp_name = self.path.file_name().map_or_else(
            || "server-snapshot.tmp".to_string(),
            |name| format!("{}.tmp", name.to_string_lossy()),
        );
        temp_path.set_file_name(temp_name);
        if temp_path.exists() {
            std::fs::remove_file(temp_path)?;
        }
        Ok(())
    }
}

fn snapshot_checksum_v4(snapshot: &SnapshotV4) -> Result<u64, serde_json::Error> {
    let bytes = serde_json::to_vec(snapshot)?;
    Ok(fnv1a64(&bytes))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn validate_session_snapshot_core(
    sessions: &[SessionSnapshotV3],
    follows: &[FollowEdgeSnapshotV2],
    selected_sessions: &[ClientSelectedSessionSnapshotV2],
) -> Result<(), SnapshotError> {
    let session_ids = validate_session_entries(sessions)?;
    validate_follow_edges(follows)?;
    validate_selected_sessions(selected_sessions, &session_ids)
}

fn validate_session_entries(
    sessions: &[SessionSnapshotV3],
) -> Result<BTreeSet<Uuid>, SnapshotError> {
    let mut session_ids = BTreeSet::new();
    let mut all_pane_ids = BTreeSet::new();
    let mut surface_ids = BTreeSet::new();

    for session in sessions {
        if !session_ids.insert(session.id) {
            return Err(SnapshotError::Validation(format!(
                "duplicate session id {}",
                session.id
            )));
        }

        if session.panes.is_empty() {
            return Err(SnapshotError::Validation(format!(
                "session {} must contain at least one pane",
                session.id
            )));
        }
        let session_pane_ids = session
            .panes
            .iter()
            .map(|pane| pane.id)
            .collect::<BTreeSet<_>>();
        validate_panes(session, &mut all_pane_ids)?;
        if let Some(focused_pane_id) = session.focused_pane_id
            && !session_pane_ids.contains(&focused_pane_id)
        {
            return Err(SnapshotError::Validation(format!(
                "focused pane {} missing from session {}",
                focused_pane_id, session.id
            )));
        }
        if let Some(layout_root) = &session.layout_root {
            let mut layout_pane_ids = BTreeSet::new();
            collect_layout_pane_ids(layout_root, &mut layout_pane_ids)?;
            if layout_pane_ids != session_pane_ids {
                return Err(SnapshotError::Validation(format!(
                    "layout panes do not match pane set for session {}",
                    session.id
                )));
            }
        }

        validate_floating_surfaces(session, &session_pane_ids, &mut surface_ids)?;
    }

    Ok(session_ids)
}

fn validate_panes(
    session: &SessionSnapshotV3,
    all_pane_ids: &mut BTreeSet<Uuid>,
) -> Result<(), SnapshotError> {
    for pane in &session.panes {
        if !all_pane_ids.insert(pane.id) {
            return Err(SnapshotError::Validation(format!(
                "pane id {} reused across sessions",
                pane.id
            )));
        }
        if pane.shell.trim().is_empty() {
            return Err(SnapshotError::Validation(format!(
                "pane {} in session {} has empty shell",
                pane.id, session.id
            )));
        }
        validate_pane_resurrection_fields(session.id, pane)?;
    }

    Ok(())
}

fn validate_pane_resurrection_fields(
    session_id: Uuid,
    pane: &PaneSnapshotV2,
) -> Result<(), SnapshotError> {
    if let Some(command) = pane.active_command.as_deref() {
        if command.trim().is_empty() {
            return Err(SnapshotError::Validation(format!(
                "pane {} in session {} has empty active command",
                pane.id, session_id
            )));
        }
        if command.len() > MAX_SNAPSHOT_COMMAND_LEN {
            return Err(SnapshotError::Validation(format!(
                "pane {} in session {} active command exceeds {} bytes",
                pane.id, session_id, MAX_SNAPSHOT_COMMAND_LEN
            )));
        }
        if pane.active_command_source.is_none() {
            return Err(SnapshotError::Validation(format!(
                "pane {} in session {} missing active command source",
                pane.id, session_id
            )));
        }
    }
    if pane.active_command_source.is_some() && pane.active_command.is_none() {
        return Err(SnapshotError::Validation(format!(
            "pane {} in session {} has command source without active command",
            pane.id, session_id
        )));
    }
    if let Some(cwd) = pane.last_known_cwd.as_deref() {
        if cwd.trim().is_empty() {
            return Err(SnapshotError::Validation(format!(
                "pane {} in session {} has empty cwd",
                pane.id, session_id
            )));
        }
        if cwd.len() > MAX_SNAPSHOT_CWD_LEN {
            return Err(SnapshotError::Validation(format!(
                "pane {} in session {} cwd exceeds {} bytes",
                pane.id, session_id, MAX_SNAPSHOT_CWD_LEN
            )));
        }
    }

    Ok(())
}

fn validate_floating_surfaces(
    session: &SessionSnapshotV3,
    session_pane_ids: &BTreeSet<Uuid>,
    surface_ids: &mut BTreeSet<Uuid>,
) -> Result<(), SnapshotError> {
    for surface in &session.floating_surfaces {
        if !surface_ids.insert(surface.id) {
            return Err(SnapshotError::Validation(format!(
                "duplicate floating surface id {}",
                surface.id
            )));
        }
        if !session_pane_ids.contains(&surface.pane_id) {
            return Err(SnapshotError::Validation(format!(
                "floating surface {} references missing pane {} in session {}",
                surface.id, surface.pane_id, session.id
            )));
        }
        if surface.w == 0 || surface.h == 0 {
            return Err(SnapshotError::Validation(format!(
                "floating surface {} in session {} has zero-sized rect",
                surface.id, session.id
            )));
        }
    }

    Ok(())
}

fn validate_follow_edges(follows: &[FollowEdgeSnapshotV2]) -> Result<(), SnapshotError> {
    for follow in follows {
        if follow.follower_client_id == follow.leader_client_id {
            return Err(SnapshotError::Validation(format!(
                "follow edge cannot self-reference client {}",
                follow.follower_client_id
            )));
        }
    }
    Ok(())
}

fn validate_selected_sessions(
    selected_sessions: &[ClientSelectedSessionSnapshotV2],
    session_ids: &BTreeSet<Uuid>,
) -> Result<(), SnapshotError> {
    for selected in selected_sessions {
        if let Some(session_id) = selected.session_id
            && !session_ids.contains(&session_id)
        {
            return Err(SnapshotError::Validation(format!(
                "selected session references missing session {} for client {}",
                session_id, selected.client_id
            )));
        }
    }
    Ok(())
}

fn validate_snapshot_v4(snapshot: &SnapshotV4) -> Result<(), SnapshotError> {
    validate_session_snapshot_core(
        &snapshot.sessions,
        &snapshot.follows,
        &snapshot.selected_sessions,
    )?;

    let session_ids = snapshot
        .sessions
        .iter()
        .map(|session| session.id)
        .collect::<BTreeSet<_>>();
    let context_ids = snapshot
        .contexts
        .iter()
        .map(|context| context.id)
        .collect::<BTreeSet<_>>();

    if context_ids.len() != snapshot.contexts.len() {
        return Err(SnapshotError::Validation(
            "duplicate context id in snapshot".to_string(),
        ));
    }

    for binding in &snapshot.context_session_bindings {
        if !context_ids.contains(&binding.context_id) {
            return Err(SnapshotError::Validation(format!(
                "context/session binding references missing context {}",
                binding.context_id
            )));
        }
        if !session_ids.contains(&binding.session_id) {
            return Err(SnapshotError::Validation(format!(
                "context/session binding references missing session {}",
                binding.session_id
            )));
        }
    }

    for selected in &snapshot.selected_contexts {
        if let Some(context_id) = selected.context_id
            && !context_ids.contains(&context_id)
        {
            return Err(SnapshotError::Validation(format!(
                "selected context references missing context {} for client {}",
                context_id, selected.client_id
            )));
        }
    }

    for context_id in &snapshot.mru_contexts {
        if !context_ids.contains(context_id) {
            return Err(SnapshotError::Validation(format!(
                "mru context references missing context {context_id}"
            )));
        }
    }

    Ok(())
}

fn collect_layout_pane_ids(
    node: &PaneLayoutNodeSnapshotV2,
    out: &mut BTreeSet<Uuid>,
) -> Result<(), SnapshotError> {
    match node {
        PaneLayoutNodeSnapshotV2::Leaf { pane_id } => {
            if !out.insert(*pane_id) {
                return Err(SnapshotError::Validation(format!(
                    "duplicate pane id {pane_id} in layout"
                )));
            }
        }
        PaneLayoutNodeSnapshotV2::Split {
            ratio,
            first,
            second,
            ..
        } => {
            if !(0.1..=0.9).contains(ratio) {
                return Err(SnapshotError::Validation(format!(
                    "split ratio {ratio} out of range [0.1, 0.9]"
                )));
            }
            collect_layout_pane_ids(first, out)?;
            collect_layout_pane_ids(second, out)?;
        }
    }
    Ok(())
}

const fn normalize_snapshot_v4_numbering(snapshot: SnapshotV4) -> SnapshotV4 {
    snapshot
}

#[cfg(test)]
mod tests {
    use super::{
        ClientSelectedContextSnapshotV1, ClientSelectedSessionSnapshotV2,
        ContextSessionBindingSnapshotV1, ContextSnapshotV1, FollowEdgeSnapshotV2,
        PaneLayoutNodeSnapshotV2, PaneSnapshotV2, SessionSnapshotV3, SnapshotError,
        SnapshotManager, SnapshotV4,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    #[test]
    fn snapshot_roundtrip_with_stable_ids() {
        let session_id = Uuid::new_v4();
        let window_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let leader_id = Uuid::new_v4();

        let snapshot = SnapshotV4 {
            sessions: vec![SessionSnapshotV3 {
                id: session_id,
                name: Some("dev".to_string()),
                panes: vec![PaneSnapshotV2 {
                    id: window_id,
                    name: Some("pane-1".to_string()),
                    shell: "/bin/sh".to_string(),
                    process_group_id: None,
                    active_command: None,
                    active_command_source: None,
                    last_known_cwd: None,
                }],
                focused_pane_id: Some(window_id),
                layout_root: Some(PaneLayoutNodeSnapshotV2::Leaf { pane_id: window_id }),
                floating_surfaces: vec![],
            }],
            follows: vec![FollowEdgeSnapshotV2 {
                follower_client_id: client_id,
                leader_client_id: leader_id,
                global: true,
            }],
            selected_sessions: vec![ClientSelectedSessionSnapshotV2 {
                client_id,
                session_id: Some(session_id),
            }],
            contexts: vec![ContextSnapshotV1 {
                id: session_id,
                name: Some("dev".to_string()),
                attributes: BTreeMap::from([(
                    "bmux.session_id".to_string(),
                    session_id.to_string(),
                )]),
            }],
            context_session_bindings: vec![ContextSessionBindingSnapshotV1 {
                context_id: session_id,
                session_id,
            }],
            selected_contexts: vec![ClientSelectedContextSnapshotV1 {
                client_id,
                context_id: Some(session_id),
            }],
            mru_contexts: vec![session_id],
        };

        let encoded = SnapshotManager::encode_snapshot(&snapshot).expect("snapshot should encode");
        let decoded = SnapshotManager::decode_snapshot(&encoded).expect("snapshot should decode");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.sessions[0].id, session_id);
        assert_eq!(decoded.sessions[0].panes[0].id, window_id);
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let payload = serde_json::json!({
            "version": 999,
            "checksum": 0,
            "snapshot": {
                "sessions": [],
                "follows": [],
                "selected_sessions": []
            }
        });

        let bytes = serde_json::to_vec(&payload).expect("json should encode");
        let error = SnapshotManager::decode_snapshot(&bytes).expect_err("should reject version");
        assert!(matches!(error, SnapshotError::UnsupportedVersion(999)));
    }

    #[test]
    fn decode_rejects_invalid_references() {
        let snapshot = SnapshotV4 {
            sessions: vec![SessionSnapshotV3 {
                id: Uuid::new_v4(),
                name: Some("valid".to_string()),
                panes: vec![PaneSnapshotV2 {
                    id: Uuid::new_v4(),
                    name: Some("pane-1".to_string()),
                    shell: "/bin/sh".to_string(),
                    process_group_id: None,
                    active_command: None,
                    active_command_source: None,
                    last_known_cwd: None,
                }],
                focused_pane_id: None,
                layout_root: None,
                floating_surfaces: vec![],
            }],
            follows: vec![],
            selected_sessions: vec![],
            contexts: vec![],
            context_session_bindings: vec![],
            selected_contexts: vec![],
            mru_contexts: vec![],
        };
        let encoded = SnapshotManager::encode_snapshot(&snapshot).expect("snapshot should encode");
        let mut payload: serde_json::Value =
            serde_json::from_slice(&encoded).expect("payload should decode");
        let bogus_session_id = Uuid::new_v4().to_string();
        payload["snapshot"]["selected_sessions"] = serde_json::json!([{
            "client_id": Uuid::new_v4(),
            "session_id": bogus_session_id,
        }]);
        let snapshot_bytes = serde_json::to_vec(&payload["snapshot"]).expect("snapshot bytes");
        let checksum = {
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for byte in snapshot_bytes {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
            hash
        };
        payload["checksum"] = serde_json::json!(checksum);

        let bytes = serde_json::to_vec(&payload).expect("json should encode");
        let error = SnapshotManager::decode_snapshot(&bytes).expect_err("should reject references");
        assert!(matches!(error, SnapshotError::Validation(_)));
    }

    #[test]
    fn decode_v1_is_rejected_after_hard_cut() {
        let payload = serde_json::json!({
            "version": 1,
            "checksum": 0,
            "snapshot": {},
        });

        let bytes = serde_json::to_vec(&payload).expect("json should encode");
        let error = SnapshotManager::decode_snapshot(&bytes).expect_err("legacy snapshot rejected");
        assert!(matches!(error, SnapshotError::UnsupportedVersion(1)));
    }

    #[test]
    fn encode_rejects_command_source_without_command() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let snapshot = SnapshotV4 {
            sessions: vec![SessionSnapshotV3 {
                id: session_id,
                name: Some("dev".to_string()),
                panes: vec![PaneSnapshotV2 {
                    id: pane_id,
                    name: Some("pane-1".to_string()),
                    shell: "/bin/sh".to_string(),
                    process_group_id: None,
                    active_command: None,
                    active_command_source: Some(crate::PaneCommandSource::Verbatim),
                    last_known_cwd: Some("/tmp".to_string()),
                }],
                focused_pane_id: Some(pane_id),
                layout_root: Some(PaneLayoutNodeSnapshotV2::Leaf { pane_id }),
                floating_surfaces: vec![],
            }],
            follows: vec![],
            selected_sessions: vec![],
            contexts: vec![ContextSnapshotV1 {
                id: session_id,
                name: Some("dev".to_string()),
                attributes: BTreeMap::from([(
                    "bmux.session_id".to_string(),
                    session_id.to_string(),
                )]),
            }],
            context_session_bindings: vec![ContextSessionBindingSnapshotV1 {
                context_id: session_id,
                session_id,
            }],
            selected_contexts: vec![],
            mru_contexts: vec![session_id],
        };

        let error = SnapshotManager::encode_snapshot(&snapshot)
            .expect_err("source without command should fail validation");
        assert!(matches!(error, SnapshotError::Validation(_)));
    }
}
