use bmux_config::ConfigPaths;
use bmux_ipc::SessionRole;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

const SNAPSHOT_VERSION_V1: u32 = 1;

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotV1 {
    pub sessions: Vec<SessionSnapshotV1>,
    pub roles: Vec<RoleAssignmentSnapshotV1>,
    pub follows: Vec<FollowEdgeSnapshotV1>,
    pub selected_sessions: Vec<ClientSelectedSessionSnapshotV1>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionSnapshotV1 {
    pub id: Uuid,
    pub name: Option<String>,
    pub windows: Vec<WindowSnapshotV1>,
    pub active_window_id: Option<Uuid>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WindowSnapshotV1 {
    pub id: Uuid,
    pub name: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RoleAssignmentSnapshotV1 {
    pub session_id: Uuid,
    pub client_id: Uuid,
    pub role: SessionRole,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FollowEdgeSnapshotV1 {
    pub follower_client_id: Uuid,
    pub leader_client_id: Uuid,
    pub global: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ClientSelectedSessionSnapshotV1 {
    pub client_id: Uuid,
    pub session_id: Option<Uuid>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SnapshotEnvelopeV1 {
    version: u32,
    snapshot: SnapshotV1,
}

#[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct SnapshotManager {
    path: PathBuf,
}

#[allow(dead_code)]
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
    pub(crate) fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn encode_snapshot(snapshot: &SnapshotV1) -> Result<Vec<u8>, SnapshotError> {
        validate_snapshot(snapshot)?;
        let envelope = SnapshotEnvelopeV1 {
            version: SNAPSHOT_VERSION_V1,
            snapshot: snapshot.clone(),
        };
        serde_json::to_vec_pretty(&envelope).map_err(SnapshotError::Encode)
    }

    pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<SnapshotV1, SnapshotError> {
        let envelope: SnapshotEnvelopeV1 = serde_json::from_slice(bytes)?;
        if envelope.version != SNAPSHOT_VERSION_V1 {
            return Err(SnapshotError::UnsupportedVersion(envelope.version));
        }
        validate_snapshot(&envelope.snapshot)?;
        Ok(envelope.snapshot)
    }

    pub(crate) fn write_snapshot(&self, snapshot: &SnapshotV1) -> Result<(), SnapshotError> {
        let encoded = Self::encode_snapshot(snapshot)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, encoded)?;
        Ok(())
    }

    pub(crate) fn read_snapshot(&self) -> Result<SnapshotV1, SnapshotError> {
        let bytes = std::fs::read(&self.path)?;
        Self::decode_snapshot(&bytes)
    }
}

#[allow(dead_code)]
fn validate_snapshot(snapshot: &SnapshotV1) -> Result<(), SnapshotError> {
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

#[cfg(test)]
mod tests {
    use super::{
        ClientSelectedSessionSnapshotV1, FollowEdgeSnapshotV1, RoleAssignmentSnapshotV1,
        SessionSnapshotV1, SnapshotError, SnapshotManager, SnapshotV1, WindowSnapshotV1,
    };
    use bmux_ipc::SessionRole;
    use uuid::Uuid;

    #[test]
    fn snapshot_roundtrip_with_stable_ids() {
        let session_id = Uuid::new_v4();
        let window_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
        let leader_id = Uuid::new_v4();

        let snapshot = SnapshotV1 {
            sessions: vec![SessionSnapshotV1 {
                id: session_id,
                name: Some("dev".to_string()),
                windows: vec![WindowSnapshotV1 {
                    id: window_id,
                    name: Some("main".to_string()),
                }],
                active_window_id: Some(window_id),
            }],
            roles: vec![RoleAssignmentSnapshotV1 {
                session_id,
                client_id,
                role: SessionRole::Owner,
            }],
            follows: vec![FollowEdgeSnapshotV1 {
                follower_client_id: client_id,
                leader_client_id: leader_id,
                global: true,
            }],
            selected_sessions: vec![ClientSelectedSessionSnapshotV1 {
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
        let session_id = Uuid::new_v4();
        let payload = serde_json::json!({
            "version": 1,
            "snapshot": {
                "sessions": [
                    {
                        "id": session_id,
                        "name": null,
                        "windows": [],
                        "active_window_id": null
                    }
                ],
                "roles": [],
                "follows": [],
                "selected_sessions": [
                    {
                        "client_id": Uuid::new_v4(),
                        "session_id": Uuid::new_v4()
                    }
                ]
            }
        });

        let bytes = serde_json::to_vec(&payload).expect("json should encode");
        let error = SnapshotManager::decode_snapshot(&bytes).expect_err("should reject references");
        assert!(matches!(error, SnapshotError::Validation(_)));
    }
}
