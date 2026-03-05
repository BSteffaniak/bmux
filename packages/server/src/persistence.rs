use bmux_config::ConfigPaths;
use bmux_ipc::SessionRole;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

const SNAPSHOT_VERSION_V1: u32 = 1;
const SNAPSHOT_VERSION_V2: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SnapshotV2 {
    pub sessions: Vec<SessionSnapshotV2>,
    #[serde(default)]
    pub owner_principals: Vec<OwnerPrincipalSnapshotV2>,
    pub roles: Vec<RoleAssignmentSnapshotV2>,
    pub follows: Vec<FollowEdgeSnapshotV2>,
    pub selected_sessions: Vec<ClientSelectedSessionSnapshotV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OwnerPrincipalSnapshotV2 {
    pub session_id: Uuid,
    pub principal_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionSnapshotV2 {
    pub id: Uuid,
    pub name: Option<String>,
    pub windows: Vec<WindowSnapshotV2>,
    pub active_window_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct WindowSnapshotV2 {
    pub id: Uuid,
    pub name: Option<String>,
    pub panes: Vec<PaneSnapshotV2>,
    pub focused_pane_id: Option<Uuid>,
    #[serde(default)]
    pub layout_root: Option<PaneLayoutNodeSnapshotV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PaneSnapshotV2 {
    pub id: Uuid,
    pub name: Option<String>,
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PaneLayoutNodeSnapshotV2 {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneSplitDirectionSnapshotV2,
        ratio: f32,
        first: Box<PaneLayoutNodeSnapshotV2>,
        second: Box<PaneLayoutNodeSnapshotV2>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PaneSplitDirectionSnapshotV2 {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RoleAssignmentSnapshotV2 {
    pub session_id: Uuid,
    pub client_id: Uuid,
    pub role: SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FollowEdgeSnapshotV2 {
    pub follower_client_id: Uuid,
    pub leader_client_id: Uuid,
    pub global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClientSelectedSessionSnapshotV2 {
    pub client_id: Uuid,
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotV1 {
    sessions: Vec<SessionSnapshotV1>,
    roles: Vec<RoleAssignmentSnapshotV1>,
    follows: Vec<FollowEdgeSnapshotV1>,
    selected_sessions: Vec<ClientSelectedSessionSnapshotV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SessionSnapshotV1 {
    id: Uuid,
    name: Option<String>,
    windows: Vec<WindowSnapshotV1>,
    active_window_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct WindowSnapshotV1 {
    id: Uuid,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RoleAssignmentSnapshotV1 {
    session_id: Uuid,
    client_id: Uuid,
    role: SessionRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FollowEdgeSnapshotV1 {
    follower_client_id: Uuid,
    leader_client_id: Uuid,
    global: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClientSelectedSessionSnapshotV1 {
    client_id: Uuid,
    session_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotEnvelopeV1 {
    version: u32,
    checksum: u64,
    snapshot: SnapshotV1,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SnapshotEnvelopeV2 {
    version: u32,
    checksum: u64,
    snapshot: SnapshotV2,
}

#[derive(Debug, Error)]
pub(crate) enum SnapshotError {
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
pub(crate) struct SnapshotManager {
    path: PathBuf,
}

impl SnapshotManager {
    pub(crate) fn from_paths(paths: &ConfigPaths) -> Self {
        Self {
            path: paths
                .data_dir
                .join("runtime")
                .join("server-snapshot-v1.json"),
        }
    }

    #[must_use]
    #[allow(dead_code)]
    pub(crate) fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn encode_snapshot(snapshot: &SnapshotV2) -> Result<Vec<u8>, SnapshotError> {
        validate_snapshot(snapshot)?;
        let checksum = snapshot_checksum(snapshot).map_err(SnapshotError::Encode)?;
        let envelope = SnapshotEnvelopeV2 {
            version: SNAPSHOT_VERSION_V2,
            checksum,
            snapshot: snapshot.clone(),
        };
        serde_json::to_vec_pretty(&envelope).map_err(SnapshotError::Encode)
    }

    pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotV2, SnapshotError> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let version = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| SnapshotError::Validation("snapshot missing version".to_string()))?;

        match version as u32 {
            SNAPSHOT_VERSION_V1 => {
                let envelope: SnapshotEnvelopeV1 = serde_json::from_value(value)?;
                let expected_checksum =
                    snapshot_checksum_v1(&envelope.snapshot).map_err(SnapshotError::Encode)?;
                if expected_checksum != envelope.checksum {
                    return Err(SnapshotError::Validation(
                        "snapshot checksum mismatch".to_string(),
                    ));
                }
                validate_snapshot_v1(&envelope.snapshot)?;
                Ok(upgrade_snapshot_v1(envelope.snapshot))
            }
            SNAPSHOT_VERSION_V2 => {
                let envelope: SnapshotEnvelopeV2 = serde_json::from_value(value)?;
                let expected_checksum =
                    snapshot_checksum(&envelope.snapshot).map_err(SnapshotError::Encode)?;
                if expected_checksum != envelope.checksum {
                    return Err(SnapshotError::Validation(
                        "snapshot checksum mismatch".to_string(),
                    ));
                }
                validate_snapshot(&envelope.snapshot)?;
                Ok(envelope.snapshot)
            }
            other => Err(SnapshotError::UnsupportedVersion(other)),
        }
    }

    pub(crate) fn write_snapshot(&self, snapshot: &SnapshotV2) -> Result<(), SnapshotError> {
        let encoded = Self::encode_snapshot(snapshot)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut temp_path = self.path.clone();
        let temp_name = match self.path.file_name() {
            Some(name) => format!("{}.tmp", name.to_string_lossy()),
            None => "server-snapshot.tmp".to_string(),
        };
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

    pub(crate) fn read_snapshot(&self) -> Result<SnapshotV2, SnapshotError> {
        let bytes = std::fs::read(&self.path)?;
        Self::decode_snapshot(&bytes)
    }

    pub(crate) fn cleanup_temp_file(&self) -> Result<(), SnapshotError> {
        let mut temp_path = self.path.clone();
        let temp_name = match self.path.file_name() {
            Some(name) => format!("{}.tmp", name.to_string_lossy()),
            None => "server-snapshot.tmp".to_string(),
        };
        temp_path.set_file_name(temp_name);
        if temp_path.exists() {
            std::fs::remove_file(temp_path)?;
        }
        Ok(())
    }
}

fn snapshot_checksum(snapshot: &SnapshotV2) -> Result<u64, serde_json::Error> {
    let bytes = serde_json::to_vec(snapshot)?;
    Ok(fnv1a64(&bytes))
}

fn snapshot_checksum_v1(snapshot: &SnapshotV1) -> Result<u64, serde_json::Error> {
    let bytes = serde_json::to_vec(snapshot)?;
    Ok(fnv1a64(&bytes))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn validate_snapshot(snapshot: &SnapshotV2) -> Result<(), SnapshotError> {
    let mut session_ids = BTreeSet::new();
    let mut all_window_ids = BTreeSet::new();
    let mut all_pane_ids = BTreeSet::new();

    for session in &snapshot.sessions {
        if !session_ids.insert(session.id) {
            return Err(SnapshotError::Validation(format!(
                "duplicate session id {}",
                session.id
            )));
        }

        let mut session_window_ids = BTreeSet::new();
        for window in &session.windows {
            if !session_window_ids.insert(window.id) {
                return Err(SnapshotError::Validation(format!(
                    "duplicate window id {} in session {}",
                    window.id, session.id
                )));
            }
            if !all_window_ids.insert(window.id) {
                return Err(SnapshotError::Validation(format!(
                    "window id {} reused across sessions",
                    window.id
                )));
            }

            let mut window_pane_ids = BTreeSet::new();
            for pane in &window.panes {
                if !window_pane_ids.insert(pane.id) {
                    return Err(SnapshotError::Validation(format!(
                        "duplicate pane id {} in window {}",
                        pane.id, window.id
                    )));
                }
                if !all_pane_ids.insert(pane.id) {
                    return Err(SnapshotError::Validation(format!(
                        "pane id {} reused across windows",
                        pane.id
                    )));
                }
                if pane.shell.trim().is_empty() {
                    return Err(SnapshotError::Validation(format!(
                        "pane {} in window {} has empty shell",
                        pane.id, window.id
                    )));
                }
            }

            if window.panes.is_empty() {
                return Err(SnapshotError::Validation(format!(
                    "window {} must contain at least one pane",
                    window.id
                )));
            }

            if let Some(focused_pane_id) = window.focused_pane_id
                && !window_pane_ids.contains(&focused_pane_id)
            {
                return Err(SnapshotError::Validation(format!(
                    "focused pane {} missing from window {}",
                    focused_pane_id, window.id
                )));
            }

            if let Some(layout_root) = &window.layout_root {
                let mut layout_pane_ids = BTreeSet::new();
                collect_layout_pane_ids(layout_root, &mut layout_pane_ids)?;
                if layout_pane_ids != window_pane_ids {
                    return Err(SnapshotError::Validation(format!(
                        "layout panes do not match pane set for window {}",
                        window.id
                    )));
                }
            }
        }

        if let Some(active_window_id) = session.active_window_id
            && !session_window_ids.contains(&active_window_id)
        {
            return Err(SnapshotError::Validation(format!(
                "active window {} missing from session {}",
                active_window_id, session.id
            )));
        }
    }

    for role in &snapshot.roles {
        if !session_ids.contains(&role.session_id) {
            return Err(SnapshotError::Validation(format!(
                "role assignment references missing session {}",
                role.session_id
            )));
        }
    }

    let mut owner_sessions = BTreeSet::new();
    for owner in &snapshot.owner_principals {
        if !session_ids.contains(&owner.session_id) {
            return Err(SnapshotError::Validation(format!(
                "owner principal references missing session {}",
                owner.session_id
            )));
        }
        if !owner_sessions.insert(owner.session_id) {
            return Err(SnapshotError::Validation(format!(
                "duplicate owner principal assignment for session {}",
                owner.session_id
            )));
        }
    }

    for follow in &snapshot.follows {
        if follow.follower_client_id == follow.leader_client_id {
            return Err(SnapshotError::Validation(format!(
                "follow edge cannot self-reference client {}",
                follow.follower_client_id
            )));
        }
    }

    for selected in &snapshot.selected_sessions {
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

fn validate_snapshot_v1(snapshot: &SnapshotV1) -> Result<(), SnapshotError> {
    let mut session_ids = BTreeSet::new();
    let mut all_window_ids = BTreeSet::new();

    for session in &snapshot.sessions {
        if !session_ids.insert(session.id) {
            return Err(SnapshotError::Validation(format!(
                "duplicate session id {}",
                session.id
            )));
        }

        let mut session_window_ids = BTreeSet::new();
        for window in &session.windows {
            if !session_window_ids.insert(window.id) {
                return Err(SnapshotError::Validation(format!(
                    "duplicate window id {} in session {}",
                    window.id, session.id
                )));
            }
            if !all_window_ids.insert(window.id) {
                return Err(SnapshotError::Validation(format!(
                    "window id {} reused across sessions",
                    window.id
                )));
            }
        }

        if let Some(active_window_id) = session.active_window_id
            && !session_window_ids.contains(&active_window_id)
        {
            return Err(SnapshotError::Validation(format!(
                "active window {} missing from session {}",
                active_window_id, session.id
            )));
        }
    }

    for role in &snapshot.roles {
        if !session_ids.contains(&role.session_id) {
            return Err(SnapshotError::Validation(format!(
                "role assignment references missing session {}",
                role.session_id
            )));
        }
    }

    for follow in &snapshot.follows {
        if follow.follower_client_id == follow.leader_client_id {
            return Err(SnapshotError::Validation(format!(
                "follow edge cannot self-reference client {}",
                follow.follower_client_id
            )));
        }
    }

    for selected in &snapshot.selected_sessions {
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

fn collect_layout_pane_ids(
    node: &PaneLayoutNodeSnapshotV2,
    out: &mut BTreeSet<Uuid>,
) -> Result<(), SnapshotError> {
    match node {
        PaneLayoutNodeSnapshotV2::Leaf { pane_id } => {
            if !out.insert(*pane_id) {
                return Err(SnapshotError::Validation(format!(
                    "duplicate pane id {} in layout",
                    pane_id
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
                    "split ratio {} out of range [0.1, 0.9]",
                    ratio
                )));
            }
            collect_layout_pane_ids(first, out)?;
            collect_layout_pane_ids(second, out)?;
        }
    }
    Ok(())
}

fn upgrade_snapshot_v1(snapshot: SnapshotV1) -> SnapshotV2 {
    SnapshotV2 {
        sessions: snapshot
            .sessions
            .into_iter()
            .map(|session| SessionSnapshotV2 {
                id: session.id,
                name: session.name,
                windows: session
                    .windows
                    .into_iter()
                    .map(|window| WindowSnapshotV2 {
                        id: window.id,
                        name: window.name,
                        panes: vec![PaneSnapshotV2 {
                            id: window.id,
                            name: Some("pane-1".to_string()),
                            shell: default_shell_for_upgrade(),
                        }],
                        focused_pane_id: Some(window.id),
                        layout_root: Some(PaneLayoutNodeSnapshotV2::Leaf { pane_id: window.id }),
                    })
                    .collect(),
                active_window_id: session.active_window_id,
            })
            .collect(),
        owner_principals: Vec::new(),
        roles: snapshot
            .roles
            .into_iter()
            .map(|role| RoleAssignmentSnapshotV2 {
                session_id: role.session_id,
                client_id: role.client_id,
                role: role.role,
            })
            .collect(),
        follows: snapshot
            .follows
            .into_iter()
            .map(|follow| FollowEdgeSnapshotV2 {
                follower_client_id: follow.follower_client_id,
                leader_client_id: follow.leader_client_id,
                global: follow.global,
            })
            .collect(),
        selected_sessions: snapshot
            .selected_sessions
            .into_iter()
            .map(|selected| ClientSelectedSessionSnapshotV2 {
                client_id: selected.client_id,
                session_id: selected.session_id,
            })
            .collect(),
    }
}

fn default_shell_for_upgrade() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(windows) {
            "cmd.exe".to_string()
        } else {
            "/bin/sh".to_string()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ClientSelectedSessionSnapshotV2, FollowEdgeSnapshotV2, PaneLayoutNodeSnapshotV2,
        PaneSnapshotV2, RoleAssignmentSnapshotV2, SessionSnapshotV2, SnapshotError,
        SnapshotManager, SnapshotV2, WindowSnapshotV2,
    };
    use bmux_ipc::SessionRole;
    use uuid::Uuid;

    #[test]
    fn snapshot_roundtrip_with_stable_ids() {
        let session_id = Uuid::new_v4();
        let window_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let leader_id = Uuid::new_v4();

        let snapshot = SnapshotV2 {
            sessions: vec![SessionSnapshotV2 {
                id: session_id,
                name: Some("dev".to_string()),
                windows: vec![WindowSnapshotV2 {
                    id: window_id,
                    name: Some("main".to_string()),
                    panes: vec![PaneSnapshotV2 {
                        id: window_id,
                        name: Some("pane-1".to_string()),
                        shell: "/bin/sh".to_string(),
                    }],
                    focused_pane_id: Some(window_id),
                    layout_root: Some(PaneLayoutNodeSnapshotV2::Leaf { pane_id: window_id }),
                }],
                active_window_id: Some(window_id),
            }],
            owner_principals: vec![],
            roles: vec![RoleAssignmentSnapshotV2 {
                session_id,
                client_id,
                role: SessionRole::Owner,
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
        };

        let encoded = SnapshotManager::encode_snapshot(&snapshot).expect("snapshot should encode");
        let decoded = SnapshotManager::decode_snapshot(&encoded).expect("snapshot should decode");

        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.sessions[0].id, session_id);
        assert_eq!(decoded.sessions[0].windows[0].id, window_id);
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let payload = serde_json::json!({
            "version": 999,
            "checksum": 0,
            "snapshot": {
                "sessions": [],
                "roles": [],
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
        let snapshot = SnapshotV2 {
            sessions: vec![SessionSnapshotV2 {
                id: Uuid::new_v4(),
                name: Some("valid".to_string()),
                windows: vec![WindowSnapshotV2 {
                    id: Uuid::new_v4(),
                    name: Some("w1".to_string()),
                    panes: vec![PaneSnapshotV2 {
                        id: Uuid::new_v4(),
                        name: Some("pane-1".to_string()),
                        shell: "/bin/sh".to_string(),
                    }],
                    focused_pane_id: None,
                    layout_root: None,
                }],
                active_window_id: None,
            }],
            owner_principals: vec![],
            roles: vec![],
            follows: vec![],
            selected_sessions: vec![],
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
            let mut hash = 0xcbf29ce484222325u64;
            for byte in snapshot_bytes {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            hash
        };
        payload["checksum"] = serde_json::json!(checksum);

        let bytes = serde_json::to_vec(&payload).expect("json should encode");
        let error = SnapshotManager::decode_snapshot(&bytes).expect_err("should reject references");
        assert!(matches!(error, SnapshotError::Validation(_)));
    }

    #[test]
    fn decode_v1_upgrades_window_to_single_pane_with_stable_ids() {
        let session_id = Uuid::new_v4();
        let window_id = Uuid::new_v4();
        let legacy_snapshot = super::SnapshotV1 {
            sessions: vec![super::SessionSnapshotV1 {
                id: session_id,
                name: Some("legacy".to_string()),
                windows: vec![super::WindowSnapshotV1 {
                    id: window_id,
                    name: Some("main".to_string()),
                }],
                active_window_id: Some(window_id),
            }],
            roles: vec![],
            follows: vec![],
            selected_sessions: vec![],
        };
        let checksum = super::snapshot_checksum_v1(&legacy_snapshot).expect("checksum");
        let payload = serde_json::json!({
            "version": 1,
            "checksum": checksum,
            "snapshot": legacy_snapshot,
        });

        let bytes = serde_json::to_vec(&payload).expect("json should encode");
        let decoded = SnapshotManager::decode_snapshot(&bytes).expect("legacy snapshot decodes");
        assert_eq!(decoded.sessions.len(), 1);
        assert_eq!(decoded.sessions[0].windows.len(), 1);
        let window = &decoded.sessions[0].windows[0];
        assert_eq!(window.id, window_id);
        assert_eq!(window.panes.len(), 1);
        assert_eq!(window.panes[0].id, window_id);
        assert_eq!(window.focused_pane_id, Some(window_id));
    }
}
