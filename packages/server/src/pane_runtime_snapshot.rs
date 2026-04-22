//! Server-owned pane-runtime `StatefulPlugin` participant.
//!
//! Panes, layouts, floating surfaces and per-pane resurrection fields
//! are server runtime — they live inside the process managing PTYs,
//! not inside any plugin domain. The snapshot-orchestration plugin
//! iterates every registered `StatefulPlugin` participant when
//! building/restoring a combined envelope; this module registers the
//! server-side participant so the pane runtime ends up in that
//! envelope alongside the plugin-owned slices.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use anyhow::Context;
use bmux_ipc::PaneLaunchCommand;
use bmux_plugin_sdk::{
    PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot,
};
use bmux_session_models::SessionId;
use bmux_snapshot_runtime::StatefulPluginRegistry;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use crate::{
    FloatingSurfaceRuntime, LayoutRect, PaneCommandSource, PaneLaunchSpec, PaneLayoutNode,
    PaneResurrectionSnapshot, PaneRuntimeMeta, PaneSplitDirection, ServerState,
    inspect_process_group_command_and_cwd, session_handle, validate_runtime_layout_matches_panes,
};

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

fn layout_to_snapshot(node: &PaneLayoutNode) -> PaneRuntimeSnapshotV1Layout {
    match node {
        PaneLayoutNode::Leaf { pane_id } => PaneRuntimeSnapshotV1Layout::Leaf { pane_id: *pane_id },
        PaneLayoutNode::Split {
            direction,
            ratio,
            first,
            second,
        } => PaneRuntimeSnapshotV1Layout::Split {
            direction: match direction {
                PaneSplitDirection::Vertical => PaneRuntimeSnapshotV1SplitDirection::Vertical,
                PaneSplitDirection::Horizontal => PaneRuntimeSnapshotV1SplitDirection::Horizontal,
            },
            ratio: *ratio,
            first: Box::new(layout_to_snapshot(first)),
            second: Box::new(layout_to_snapshot(second)),
        },
    }
}

fn layout_from_snapshot(node: &PaneRuntimeSnapshotV1Layout) -> PaneLayoutNode {
    match node {
        PaneRuntimeSnapshotV1Layout::Leaf { pane_id } => PaneLayoutNode::Leaf { pane_id: *pane_id },
        PaneRuntimeSnapshotV1Layout::Split {
            direction,
            ratio,
            first,
            second,
        } => PaneLayoutNode::Split {
            direction: match direction {
                PaneRuntimeSnapshotV1SplitDirection::Vertical => PaneSplitDirection::Vertical,
                PaneRuntimeSnapshotV1SplitDirection::Horizontal => PaneSplitDirection::Horizontal,
            },
            ratio: *ratio,
            first: Box::new(layout_from_snapshot(first)),
            second: Box::new(layout_from_snapshot(second)),
        },
    }
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
            || registry.get::<StatefulPluginRegistry>(),
            |fresh| {
                registry.register::<StatefulPluginRegistry>(fresh);
            },
        );
        if let Ok(mut guard) = stateful_registry.write() {
            guard.push(handle);
        }
    }
}

/// Walk `state.session_runtimes` and produce a real pane-runtime
/// payload for persistence.
#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
fn build_pane_runtime_payload(state: &Arc<ServerState>) -> anyhow::Result<PaneRuntimeSnapshotV1> {
    let sessions = session_handle().0.list_sessions();

    let runtime_manager = state
        .session_runtimes
        .lock()
        .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;

    let mut out = Vec::with_capacity(sessions.len());
    for session_info in sessions {
        let Some(runtime) = runtime_manager.runtimes.get(&session_info.id) else {
            continue;
        };

        validate_runtime_layout_matches_panes(&runtime.layout_root, &runtime.panes).with_context(
            || {
                format!(
                    "cannot snapshot inconsistent layout for session {}",
                    session_info.id.0
                )
            },
        )?;

        let mut pane_ids = Vec::new();
        runtime.layout_root.pane_order(&mut pane_ids);
        let mut panes = Vec::with_capacity(pane_ids.len());
        for pane_id in pane_ids {
            let Some(pane) = runtime.panes.get(&pane_id) else {
                anyhow::bail!(
                    "layout references missing pane {pane_id} in session {}",
                    session_info.id.0
                );
            };
            let process_id = pane.process_id.lock().ok().and_then(|v| *v);
            let process_group_id = pane.process_group_id.lock().ok().and_then(|v| *v);
            let mut resurrection_runtime = pane
                .resurrection_state
                .lock()
                .ok()
                .map(|s| s.clone())
                .unwrap_or_default();

            if !pane.exited.load(Ordering::SeqCst)
                && resurrection_runtime.active_command_source != Some(PaneCommandSource::Verbatim)
            {
                match inspect_process_group_command_and_cwd(
                    process_group_id,
                    process_id,
                    &pane.meta.shell,
                ) {
                    Some(inspection) => {
                        if let Some(command) = inspection.command {
                            resurrection_runtime.active_command = Some(command);
                            resurrection_runtime.active_command_source =
                                Some(PaneCommandSource::Inspection);
                        } else if resurrection_runtime.active_command_source
                            == Some(PaneCommandSource::Inspection)
                        {
                            resurrection_runtime.active_command = None;
                            resurrection_runtime.active_command_source = None;
                        }
                        if let Some(cwd) = inspection.cwd {
                            resurrection_runtime.last_known_cwd = Some(cwd);
                        }
                    }
                    None if resurrection_runtime.active_command_source
                        == Some(PaneCommandSource::Inspection) =>
                    {
                        resurrection_runtime.active_command = None;
                        resurrection_runtime.active_command_source = None;
                    }
                    None => {}
                }
            }

            if let Ok(mut state_guard) = pane.resurrection_state.lock() {
                *state_guard = resurrection_runtime.clone();
            }
            let resurrection_snapshot = resurrection_runtime.to_snapshot();

            panes.push(PaneRuntimeSnapshotV1Pane {
                id: pane.meta.id,
                name: pane.meta.name.clone(),
                shell: pane.meta.shell.clone(),
                launch_command: pane.meta.launch.as_ref().map(|command| PaneLaunchCommand {
                    program: command.program.clone(),
                    args: command.args.clone(),
                    cwd: command.cwd.clone(),
                    env: command.env.clone(),
                }),
                process_group_id,
                active_command: resurrection_snapshot.active_command,
                active_command_source: resurrection_snapshot.active_command_source,
                last_known_cwd: resurrection_snapshot.last_known_cwd,
            });
        }

        let floating_surfaces = runtime
            .floating_surfaces
            .iter()
            .map(|surface| PaneRuntimeSnapshotV1FloatingSurface {
                id: surface.id,
                pane_id: surface.pane_id,
                x: surface.rect.x,
                y: surface.rect.y,
                w: surface.rect.w,
                h: surface.rect.h,
                z: surface.z,
                visible: surface.visible,
                opaque: surface.opaque,
                accepts_input: surface.accepts_input,
                cursor_owner: surface.cursor_owner,
            })
            .collect();

        out.push(PaneRuntimeSessionSnapshotV1 {
            session_id: session_info.id.0,
            panes,
            focused_pane_id: Some(runtime.focused_pane_id),
            layout_root: Some(layout_to_snapshot(&runtime.layout_root)),
            floating_surfaces,
        });
    }

    Ok(PaneRuntimeSnapshotV1 { sessions: out })
}

/// Apply a pane-runtime payload: for each session present in the
/// payload (and in the session manager at this point — which is the
/// sessions-plugin participant's responsibility, restored earlier in
/// the envelope iteration), reconstruct the pane runtime via
/// `SessionRuntimeManager::restore_runtime`.
fn apply_pane_runtime_payload(
    state: &Arc<ServerState>,
    payload: &PaneRuntimeSnapshotV1,
) -> anyhow::Result<()> {
    let session_manager = session_handle();
    let mut runtime_manager = state
        .session_runtimes
        .lock()
        .map_err(|_| anyhow::anyhow!("session runtime manager lock poisoned"))?;

    for entry in &payload.sessions {
        if entry.panes.is_empty() {
            warn!(
                "skipping pane-runtime entry for session {}: no panes to restore",
                entry.session_id
            );
            continue;
        }
        let session_id = SessionId(entry.session_id);

        // The sessions-plugin participant is iterated before us in the
        // combined envelope, so the session entry should already exist
        // in the session manager. If it doesn't, skip — there's no
        // owning session to attach the runtime to.
        if !session_manager.0.contains(session_id) {
            warn!(
                "skipping pane-runtime entry for session {}: session not in manager",
                entry.session_id
            );
            continue;
        }

        let runtime_panes = entry
            .panes
            .iter()
            .map(|pane| PaneRuntimeMeta {
                id: pane.id,
                name: pane.name.clone(),
                shell: pane.shell.clone(),
                launch: pane.launch_command.as_ref().map(|command| PaneLaunchSpec {
                    program: command.program.clone(),
                    args: command.args.clone(),
                    cwd: command.cwd.clone(),
                    env: command.env.clone(),
                }),
                resurrection: PaneResurrectionSnapshot {
                    active_command: pane.active_command.clone(),
                    active_command_source: pane.active_command_source,
                    last_known_cwd: pane.last_known_cwd.clone(),
                },
            })
            .collect::<Vec<_>>();

        let focused_pane_id = entry
            .focused_pane_id
            .or_else(|| entry.panes.first().map(|p| p.id))
            .expect("non-empty panes list guarantees a first pane");

        let floating_surfaces = entry
            .floating_surfaces
            .iter()
            .map(|surface| FloatingSurfaceRuntime {
                id: surface.id,
                pane_id: surface.pane_id,
                rect: LayoutRect {
                    x: surface.x,
                    y: surface.y,
                    w: surface.w,
                    h: surface.h,
                },
                z: surface.z,
                visible: surface.visible,
                opaque: surface.opaque,
                accepts_input: surface.accepts_input,
                cursor_owner: surface.cursor_owner,
            })
            .collect::<Vec<_>>();

        if let Err(error) = runtime_manager.restore_runtime(
            session_id,
            &runtime_panes,
            entry.layout_root.as_ref().map(layout_from_snapshot),
            focused_pane_id,
            floating_surfaces,
        ) {
            warn!(
                "failed restoring pane runtime for session {}: {error}",
                entry.session_id
            );
            // Remove the orphaned session entry so future snapshots
            // don't trip over an incomplete restore.
            let _ = session_manager.0.remove_session(session_id);
        }
    }
    Ok(())
}

impl StatefulPlugin for ServerPaneRuntimeStateful {
    fn id(&self) -> PluginEventKind {
        SERVER_PANE_RUNTIME_ID
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let Some(state) = self.state.upgrade() else {
            // Server has gone away — emit an empty payload so the
            // orchestrator can still produce a valid envelope.
            return empty_snapshot();
        };
        let payload = build_pane_runtime_payload(&state).map_err(|err| {
            StatefulPluginError::SnapshotFailed {
                plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
                details: format!("{err:#}"),
            }
        })?;
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
        let decoded: PaneRuntimeSnapshotV1 =
            serde_json::from_slice(&snapshot.bytes).map_err(|err| {
                StatefulPluginError::RestoreFailed {
                    plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
                    details: err.to_string(),
                }
            })?;
        let Some(state) = self.state.upgrade() else {
            // Server gone — nothing to restore into. Not an error.
            return Ok(());
        };
        apply_pane_runtime_payload(&state, &decoded).map_err(|err| {
            StatefulPluginError::RestoreFailed {
                plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
                details: format!("{err:#}"),
            }
        })
    }
}

fn empty_snapshot() -> StatefulPluginResult<StatefulPluginSnapshot> {
    let bytes = serde_json::to_vec(&PaneRuntimeSnapshotV1::default()).map_err(|err| {
        StatefulPluginError::SnapshotFailed {
            plugin: SERVER_PANE_RUNTIME_ID.as_str().to_string(),
            details: err.to_string(),
        }
    })?;
    Ok(StatefulPluginSnapshot::new(
        SERVER_PANE_RUNTIME_ID,
        SERVER_PANE_RUNTIME_VERSION,
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        PaneRuntimeSessionSnapshotV1, PaneRuntimeSnapshotV1, PaneRuntimeSnapshotV1FloatingSurface,
        PaneRuntimeSnapshotV1Layout, PaneRuntimeSnapshotV1Pane,
        PaneRuntimeSnapshotV1SplitDirection,
    };
    use crate::PaneCommandSource;
    use bmux_ipc::PaneLaunchCommand;
    use std::collections::BTreeMap;
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

    /// Launch-command fields (program + args + cwd + env) round-trip
    /// losslessly through a JSON encode/decode of the pane-runtime
    /// snapshot. Replaces the deleted
    /// `persistence::tests::snapshot_roundtrip_persists_launch_command`
    /// test — same invariant, new schema (pane-runtime only, sessions
    /// live in the sessions-plugin section).
    #[test]
    fn schema_round_trips_launch_command_fields() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let launch = PaneLaunchCommand {
            program: "ssh".to_string(),
            args: vec!["host-a".to_string(), "-p".to_string(), "2222".to_string()],
            cwd: Some("/srv/work".to_string()),
            env: BTreeMap::from([
                ("FOO".to_string(), "bar".to_string()),
                ("NESTED_VAR".to_string(), "value with spaces".to_string()),
            ]),
        };
        let snap = PaneRuntimeSnapshotV1 {
            sessions: vec![PaneRuntimeSessionSnapshotV1 {
                session_id,
                panes: vec![PaneRuntimeSnapshotV1Pane {
                    id: pane_id,
                    name: Some("remote-a".into()),
                    shell: "/bin/sh".into(),
                    launch_command: Some(launch.clone()),
                    process_group_id: Some(4242),
                    active_command: None,
                    active_command_source: None,
                    last_known_cwd: None,
                }],
                focused_pane_id: Some(pane_id),
                layout_root: Some(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                floating_surfaces: vec![],
            }],
        };
        let bytes = serde_json::to_vec(&snap).expect("encode");
        let decoded: PaneRuntimeSnapshotV1 = serde_json::from_slice(&bytes).expect("decode");
        let restored_launch = decoded.sessions[0].panes[0]
            .launch_command
            .as_ref()
            .expect("launch_command present after round-trip");
        assert_eq!(restored_launch.program, launch.program);
        assert_eq!(restored_launch.args, launch.args);
        assert_eq!(restored_launch.cwd, launch.cwd);
        assert_eq!(restored_launch.env, launch.env);
        assert_eq!(
            decoded.sessions[0].panes[0].process_group_id,
            Some(4242),
            "process_group_id survives round-trip"
        );
    }

    /// The legacy monolithic `SnapshotV4` schema had a cross-field
    /// invariant — `active_command_source` was required to be `None`
    /// when `active_command` was `None`, and vice versa — enforced at
    /// encode time by `validate_snapshot_v4`. The new
    /// `PaneRuntimeSnapshotV1` schema relaxes that invariant:
    /// encode/decode is purely structural, and downstream restore
    /// logic in `SessionRuntimeManager::restore_runtime` treats an
    /// orphaned command source as a harmless no-op (the pane spawns
    /// with the shell's default command).
    ///
    /// This test documents the relaxation: a pane with
    /// `active_command_source = Some(Verbatim)` and `active_command
    /// = None` must round-trip cleanly through JSON without rejection.
    #[test]
    fn schema_permits_command_source_without_command() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let snap = PaneRuntimeSnapshotV1 {
            sessions: vec![PaneRuntimeSessionSnapshotV1 {
                session_id,
                panes: vec![PaneRuntimeSnapshotV1Pane {
                    id: pane_id,
                    name: Some("pane-1".into()),
                    shell: "/bin/sh".into(),
                    launch_command: None,
                    process_group_id: None,
                    active_command: None,
                    active_command_source: Some(PaneCommandSource::Verbatim),
                    last_known_cwd: Some("/tmp".into()),
                }],
                focused_pane_id: Some(pane_id),
                layout_root: Some(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                floating_surfaces: vec![],
            }],
        };
        let bytes = serde_json::to_vec(&snap).expect("encode accepts orphan command source");
        let decoded: PaneRuntimeSnapshotV1 =
            serde_json::from_slice(&bytes).expect("decode accepts orphan command source");
        assert_eq!(decoded, snap, "orphan command source round-trips verbatim");
        assert_eq!(
            decoded.sessions[0].panes[0].active_command_source,
            Some(PaneCommandSource::Verbatim)
        );
        assert!(decoded.sessions[0].panes[0].active_command.is_none());
    }
}
