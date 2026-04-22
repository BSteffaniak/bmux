//! Server-owned pane-runtime `StatefulPlugin` participant.
//!
//! Panes, layouts, floating surfaces and per-pane resurrection fields
//! are server runtime — they live inside the process managing PTYs,
//! not inside any plugin domain. The snapshot-orchestration plugin
//! (Slice 13) iterates every registered `StatefulPlugin` participant
//! when building/restoring a combined envelope; this module registers
//! the server-side participant so the pane runtime ends up in that
//! envelope alongside the plugin-owned slices.
//!
//! # Stage 3 (additive)
//!
//! The current persistence pipeline still lives in
//! [`crate::persistence`] and still runs end-to-end. This file
//! declares the typed schema + registration path so that stages 4–5
//! of Slice 13 can migrate call-sites without churning the schema
//! separately. In Stage 3 the snapshot/restore hooks are **stub
//! no-ops**: `snapshot()` returns an empty payload and
//! `restore_snapshot()` ignores the payload. Stage 5 wires them to
//! the real `state.session_runtimes` marshaling by relocating the
//! relevant pieces of `build_snapshot` / `apply_snapshot_state`.

use std::sync::{Arc, Weak};

use bmux_ipc::PaneLaunchCommand;
use bmux_plugin_sdk::{
    PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{PaneCommandSource, ServerState};

/// Stable id for the server pane-runtime snapshot surface.
const SERVER_PANE_RUNTIME_ID: PluginEventKind =
    PluginEventKind::from_static("bmux.server/pane-runtime");

/// Current schema version for pane-runtime snapshots. Bump on any
/// breaking change to [`PaneRuntimeSnapshotV1`] or its descendants.
const SERVER_PANE_RUNTIME_VERSION: u32 = 1;

/// Combined pane-runtime snapshot — one entry per session.
///
/// The session identity itself is owned by the sessions plugin; this
/// schema tracks only the runtime overlay the server manages (PTY
/// panes, layout tree, floating surfaces, focused pane).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PaneRuntimeSnapshotV1 {
    /// Per-session pane-runtime record.
    pub sessions: Vec<PaneRuntimeSessionSnapshotV1>,
}

/// A single session's pane-runtime overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneRuntimeSessionSnapshotV1 {
    /// Backing session id (owned by sessions-plugin).
    pub session_id: Uuid,
    /// Flat list of panes.
    pub panes: Vec<PaneRuntimeSnapshotV1Pane>,
    /// Currently-focused pane, if any.
    #[serde(default)]
    pub focused_pane_id: Option<Uuid>,
    /// Layout tree. `None` indicates the session has been created but
    /// no layout is persisted yet.
    #[serde(default)]
    pub layout_root: Option<PaneRuntimeSnapshotV1Layout>,
    /// Floating surfaces anchored to panes in this session.
    #[serde(default)]
    pub floating_surfaces: Vec<PaneRuntimeSnapshotV1FloatingSurface>,
}

/// Per-pane record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRuntimeSnapshotV1Pane {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    pub shell: String,
    #[serde(default)]
    pub launch_command: Option<PaneLaunchCommand>,
    #[serde(default)]
    pub process_group_id: Option<i32>,
    #[serde(default)]
    pub active_command: Option<String>,
    #[serde(default)]
    pub active_command_source: Option<PaneCommandSource>,
    #[serde(default)]
    pub last_known_cwd: Option<String>,
}

/// Pane-layout tree node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneRuntimeSnapshotV1Layout {
    Leaf {
        pane_id: Uuid,
    },
    Split {
        direction: PaneRuntimeSnapshotV1SplitDirection,
        ratio: f32,
        first: Box<Self>,
        second: Box<Self>,
    },
}

/// Split direction for layout nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneRuntimeSnapshotV1SplitDirection {
    Vertical,
    Horizontal,
}

/// Floating-surface record anchored to a pane in its session.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRuntimeSnapshotV1FloatingSurface {
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

/// Stateful-plugin participant that marshals the server's pane runtime
/// into a [`PaneRuntimeSnapshotV1`].
///
/// Holds a `Weak<ServerState>` so the handle, which lives in the
/// plugin state registry for the process lifetime, does not keep the
/// server alive past a normal shutdown. If the weak reference has
/// expired, snapshot/restore degrade to a no-op.
pub struct ServerPaneRuntimeStateful {
    state: Weak<ServerState>,
}

impl ServerPaneRuntimeStateful {
    fn new(state: &Arc<ServerState>) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }

    /// Register a `StatefulPluginHandle` wrapping this participant in
    /// the process-wide stateful-plugin registry (creating the
    /// registry slot if absent). Intended to be called once from
    /// `BmuxServer::run_impl` after the `SessionRuntimeManager` has
    /// been constructed.
    pub fn register(state: &Arc<ServerState>) {
        let participant = Self::new(state);
        let handle = StatefulPluginHandle::new(participant);
        let registry = bmux_plugin::global_plugin_state_registry();
        let stateful_registry = bmux_snapshot_runtime::get_or_init_stateful_registry(
            || registry.get::<bmux_snapshot_runtime::StatefulPluginRegistry>(),
            |fresh| {
                registry.register::<bmux_snapshot_runtime::StatefulPluginRegistry>(fresh);
            },
        );
        if let Ok(mut guard) = stateful_registry.write() {
            guard.push(handle);
        }
    }
}

impl StatefulPlugin for ServerPaneRuntimeStateful {
    fn id(&self) -> PluginEventKind {
        SERVER_PANE_RUNTIME_ID
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        // Stage 3 stub: while `crate::persistence` still owns the end-to-end
        // snapshot pipeline, this participant emits an empty payload. Stage 5
        // replaces the body with a walk of `state.session_runtimes` that
        // produces real `PaneRuntimeSnapshotV1` data.
        let _ = self.state.upgrade();
        let payload = PaneRuntimeSnapshotV1::default();
        let bytes =
            serde_json::to_vec(&payload).map_err(|err| StatefulPluginError::SnapshotFailed {
                plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
                details: err.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(
            SERVER_PANE_RUNTIME_ID,
            SERVER_PANE_RUNTIME_VERSION,
            bytes,
        ))
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        if snapshot.version != SERVER_PANE_RUNTIME_VERSION {
            return Err(StatefulPluginError::UnsupportedVersion {
                plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
                version: snapshot.version,
                expected: vec![SERVER_PANE_RUNTIME_VERSION],
            });
        }
        // Stage 3 stub: validate decode but do not apply. Stage 5 wires this
        // up to the existing `apply_snapshot_state` pane-runtime restore.
        let _decoded: PaneRuntimeSnapshotV1 =
            serde_json::from_slice(&snapshot.bytes).map_err(|err| {
                StatefulPluginError::RestoreFailed {
                    plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
                    details: err.to_string(),
                }
            })?;
        let _ = self.state.upgrade();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PaneRuntimeSessionSnapshotV1, PaneRuntimeSnapshotV1, PaneRuntimeSnapshotV1FloatingSurface,
        PaneRuntimeSnapshotV1Layout, PaneRuntimeSnapshotV1Pane,
        PaneRuntimeSnapshotV1SplitDirection,
    };
    use uuid::Uuid;

    #[test]
    fn default_snapshot_is_empty() {
        let snap = PaneRuntimeSnapshotV1::default();
        assert!(snap.sessions.is_empty());
    }

    #[test]
    fn schema_round_trips_through_json() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let surface_id = Uuid::new_v4();
        let snap = PaneRuntimeSnapshotV1 {
            sessions: vec![PaneRuntimeSessionSnapshotV1 {
                session_id,
                panes: vec![PaneRuntimeSnapshotV1Pane {
                    id: pane_id,
                    name: Some("editor".into()),
                    shell: "/bin/sh".into(),
                    launch_command: None,
                    process_group_id: None,
                    active_command: None,
                    active_command_source: None,
                    last_known_cwd: Some("/tmp".into()),
                }],
                focused_pane_id: Some(pane_id),
                layout_root: Some(PaneRuntimeSnapshotV1Layout::Split {
                    direction: PaneRuntimeSnapshotV1SplitDirection::Vertical,
                    ratio: 0.5,
                    first: Box::new(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                    second: Box::new(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                }),
                floating_surfaces: vec![PaneRuntimeSnapshotV1FloatingSurface {
                    id: surface_id,
                    pane_id,
                    x: 1,
                    y: 2,
                    w: 40,
                    h: 10,
                    z: 0,
                    visible: true,
                    opaque: false,
                    accepts_input: true,
                    cursor_owner: false,
                }],
            }],
        };
        let bytes = serde_json::to_vec(&snap).expect("encode");
        let decoded: PaneRuntimeSnapshotV1 = serde_json::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, snap);
    }
}
