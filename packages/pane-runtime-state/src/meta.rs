//! Per-pane identity + launch + resurrection records.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Explicit launch command for a pane: program + args + cwd + env.
/// Used by `launch-pane` commands and preserved through snapshot for
/// pane resurrection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneLaunchSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Pure-data pane identity record: id, optional display name, the
/// shell to spawn, an optional explicit launch command, and
/// resurrection metadata used when restoring from snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRuntimeMeta {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    pub shell: String,
    #[serde(default)]
    pub launch: Option<PaneLaunchSpec>,
    #[serde(default)]
    pub resurrection: PaneResurrectionSnapshot,
}

/// Resurrection metadata persisted across server restarts so panes
/// can re-spawn in approximately the same state. Captured during
/// snapshot; restored into live state by the plugin's
/// `restore_runtime` path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneResurrectionSnapshot {
    #[serde(default)]
    pub active_command: Option<String>,
    #[serde(default)]
    pub active_command_source: Option<PaneCommandSource>,
    #[serde(default)]
    pub last_known_cwd: Option<String>,
}

/// Source for a pane's currently-active command, distinguishing
/// commands directly observed via shell-integration markers
/// (`Verbatim`) from commands inferred via `ps` inspection
/// (`Inspection`). Used to decide whether a snapshot's resurrection
/// record should be trusted on restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneCommandSource {
    Verbatim,
    Inspection,
}

#[cfg(test)]
mod tests {
    use super::{PaneCommandSource, PaneLaunchSpec, PaneResurrectionSnapshot};
    use std::collections::BTreeMap;

    #[test]
    fn launch_spec_round_trips_through_json() {
        let spec = PaneLaunchSpec {
            program: "ssh".into(),
            args: vec!["host-a".into(), "-p".into(), "2222".into()],
            cwd: Some("/srv".into()),
            env: BTreeMap::from([("FOO".into(), "bar".into())]),
        };
        let bytes = serde_json::to_vec(&spec).unwrap();
        let decoded: PaneLaunchSpec = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(spec, decoded);
    }

    #[test]
    fn resurrection_snapshot_round_trips() {
        let snap = PaneResurrectionSnapshot {
            active_command: Some("vim".into()),
            active_command_source: Some(PaneCommandSource::Verbatim),
            last_known_cwd: Some("/tmp".into()),
        };
        let bytes = serde_json::to_vec(&snap).unwrap();
        let decoded: PaneResurrectionSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snap, decoded);
    }

    #[test]
    fn command_source_serializes_snake_case() {
        let verbatim = serde_json::to_string(&PaneCommandSource::Verbatim).unwrap();
        assert_eq!(verbatim, "\"verbatim\"");
        let inspection = serde_json::to_string(&PaneCommandSource::Inspection).unwrap();
        assert_eq!(inspection, "\"inspection\"");
    }
}
