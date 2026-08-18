//! Pane-runtime plugin `StatefulPlugin` participant.
//!
//! Panes, layouts, floating surfaces and per-pane resurrection fields
//! belong to this plugin. The snapshot-orchestration plugin iterates
//! every registered `StatefulPlugin` participant when building/restoring
//! a combined envelope; this module registers the pane-runtime participant
//! so that state is persisted alongside the other plugin-owned slices.

use bmux_attach_layout_protocol::{PaneLaunchCommand, PaneSplitDirection};
use bmux_plugin_sdk::{
    PluginEventKind, StatefulPlugin, StatefulPluginError, StatefulPluginHandle,
    StatefulPluginResult, StatefulPluginSnapshot,
};
use bmux_session_models::{ClientId, SessionId};
use bmux_snapshot_runtime::StatefulPluginRegistry;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

use crate::padding::PanePaddingSpec;
use crate::runtime::{PanePaddingRuntimeHandle, session_handle, session_runtime_handle};
use bmux_pane_runtime_state::{
    AttachViewport, FloatingPaneLayer, FloatingPaneScope, FloatingSurfaceRuntime, LayoutRect,
    PaneCommandSource, PaneLaunchSpec, PaneLayoutNode, PaneResurrectionSnapshot, PaneRuntimeMeta,
    RestoreRuntimeRequest,
};

/// Stable id for the pane-runtime plugin snapshot surface.
const PANE_RUNTIME_PLUGIN_SNAPSHOT_ID: PluginEventKind =
    PluginEventKind::from_static("bmux.pane_runtime/pane-runtime");

/// Current schema version for pane-runtime snapshots. Bump on any
/// breaking change to [`PaneRuntimeSnapshotV1`] or its descendants.
const PANE_RUNTIME_PLUGIN_SNAPSHOT_VERSION: u32 = 1;

/// Combined pane-runtime snapshot — one entry per session.
///
/// The session identity itself is owned by the sessions plugin; this
/// schema tracks only the plugin-owned runtime overlay (PTY panes,
/// layout tree, floating surfaces, focused pane).
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
    /// Pane zoomed to fill the viewport, if any. Additive/optional so
    /// snapshots written before zoom persistence restore as unzoomed.
    #[serde(default)]
    pub zoomed_pane_id: Option<Uuid>,
    /// Layout tree. `None` indicates the session has been created but
    /// no layout is persisted yet.
    #[serde(default)]
    pub layout_root: Option<PaneRuntimeSnapshotV1Layout>,
    /// Last attached viewport, used to restore PTYs at their layout-derived size
    /// before resurrected full-screen commands start drawing.
    #[serde(default)]
    pub attach_viewport: Option<AttachViewport>,
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
    #[serde(default)]
    pub padding_override: Option<PanePaddingSpec>,
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
    #[serde(default)]
    pub anchor_pane_id: Option<Uuid>,
    #[serde(default)]
    pub context_id: Option<Uuid>,
    #[serde(default)]
    pub client_id: Option<Uuid>,
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub z: i32,
    #[serde(default)]
    pub scope: FloatingPaneScope,
    #[serde(default)]
    pub layer: FloatingPaneLayer,
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

/// Stateful-plugin participant that marshals the pane-runtime plugin's
/// runtime into a [`PaneRuntimeSnapshotV1`].
pub struct PaneRuntimeStateful;

impl PaneRuntimeStateful {
    /// Register a `StatefulPluginHandle` wrapping this participant in
    /// the process-wide stateful-plugin registry (creating the
    /// registry slot if absent). Intended to be called once from
    /// pane-runtime plugin activation after the manager has been constructed.
    pub fn register() {
        let participant = Self;
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

/// Walk the registered session-runtime handle and produce a real
/// pane-runtime payload for persistence.
fn build_pane_runtime_payload() -> anyhow::Result<PaneRuntimeSnapshotV1> {
    let sessions = session_handle().0.list_sessions();
    let runtime_manager = session_runtime_handle();
    let padding_overrides = bmux_plugin::global_plugin_state_registry()
        .get::<PanePaddingRuntimeHandle>()
        .and_then(|entry| entry.read().ok().map(|guard| (*guard).clone()))
        .and_then(|handle| handle.overrides_for_snapshot().ok())
        .unwrap_or_default();

    let mut out = Vec::with_capacity(sessions.len());
    for session_info in sessions {
        let Some(runtime) = runtime_manager
            .0
            .snapshot_session_runtime_for_persistence(session_info.id)?
        else {
            continue;
        };

        let panes = runtime
            .panes
            .into_iter()
            .map(|pane| {
                let process_group_id = runtime_manager
                    .0
                    .pane_process_identity(session_info.id, pane.id)
                    .and_then(|identity| identity.process_group_id);

                PaneRuntimeSnapshotV1Pane {
                    id: pane.id,
                    name: pane.name,
                    shell: pane.shell,
                    launch_command: pane.launch.as_ref().map(|command| PaneLaunchCommand {
                        program: command.program.clone(),
                        args: command.args.clone(),
                        cwd: command.cwd.clone(),
                        env: command.env.clone(),
                    }),
                    process_group_id,
                    active_command: pane.resurrection.active_command,
                    active_command_source: pane.resurrection.active_command_source,
                    last_known_cwd: pane.resurrection.last_known_cwd,
                    padding_override: padding_overrides.get(&(session_info.id, pane.id)).copied(),
                }
            })
            .collect();

        let floating_surfaces = runtime
            .floating_surfaces
            .into_iter()
            .map(|surface| PaneRuntimeSnapshotV1FloatingSurface {
                id: surface.id,
                pane_id: surface.pane_id,
                anchor_pane_id: surface.anchor_pane_id,
                context_id: surface.context_id,
                client_id: surface.client_id.map(|client_id| client_id.0),
                x: surface.rect.x,
                y: surface.rect.y,
                w: surface.rect.w,
                h: surface.rect.h,
                z: surface.z,
                scope: surface.scope,
                layer: surface.layer,
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
            zoomed_pane_id: runtime.zoomed_pane_id,
            layout_root: runtime.layout_root.as_ref().map(layout_to_snapshot),
            attach_viewport: runtime.attach_viewport,
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
fn apply_pane_runtime_payload(payload: &PaneRuntimeSnapshotV1) {
    let session_manager = session_handle();
    let runtime_manager = session_runtime_handle();
    let padding_handle = bmux_plugin::global_plugin_state_registry()
        .get::<PanePaddingRuntimeHandle>()
        .and_then(|entry| entry.read().ok().map(|guard| (*guard).clone()));

    for entry in &payload.sessions {
        if entry.panes.is_empty() {
            warn!(
                "skipping pane-runtime entry for session {}: no panes to restore",
                entry.session_id
            );
            continue;
        }
        let session_id = SessionId(entry.session_id);
        if let Some(handle) = &padding_handle {
            for pane in &entry.panes {
                if let Some(spec) = pane.padding_override
                    && let Err(error) = handle.install_restored_override(session_id, pane.id, spec)
                {
                    warn!(%error, pane_id = %pane.id, "failed staging restored pane padding override");
                }
            }
        }

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
                anchor_pane_id: surface.anchor_pane_id,
                context_id: surface.context_id,
                client_id: surface.client_id.map(ClientId),
                rect: LayoutRect {
                    x: surface.x,
                    y: surface.y,
                    w: surface.w,
                    h: surface.h,
                },
                z: surface.z,
                scope: surface.scope,
                layer: surface.layer,
                visible: surface.visible,
                opaque: surface.opaque,
                accepts_input: surface.accepts_input,
                cursor_owner: surface.cursor_owner,
            })
            .collect::<Vec<_>>();

        if let Err(error) = runtime_manager.0.restore_runtime(
            session_id,
            &runtime_panes,
            RestoreRuntimeRequest {
                layout_root: entry.layout_root.as_ref().map(layout_from_snapshot),
                focused_pane_id,
                zoomed_pane_id: entry.zoomed_pane_id,
                floating_surfaces,
                attach_viewport: entry.attach_viewport,
            },
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
}

impl StatefulPlugin for PaneRuntimeStateful {
    fn id(&self) -> PluginEventKind {
        PANE_RUNTIME_PLUGIN_SNAPSHOT_ID
    }

    fn snapshot(&self) -> StatefulPluginResult<StatefulPluginSnapshot> {
        let payload =
            build_pane_runtime_payload().map_err(|err| StatefulPluginError::SnapshotFailed {
                plugin: PANE_RUNTIME_PLUGIN_SNAPSHOT_ID.as_str().to_string(),
                details: format!("{err:#}"),
            })?;
        let bytes =
            serde_json::to_vec(&payload).map_err(|err| StatefulPluginError::SnapshotFailed {
                plugin: PANE_RUNTIME_PLUGIN_SNAPSHOT_ID.as_str().to_string(),
                details: err.to_string(),
            })?;
        Ok(StatefulPluginSnapshot::new(
            PANE_RUNTIME_PLUGIN_SNAPSHOT_ID,
            PANE_RUNTIME_PLUGIN_SNAPSHOT_VERSION,
            bytes,
        ))
    }

    fn restore_snapshot(&self, snapshot: StatefulPluginSnapshot) -> StatefulPluginResult<()> {
        if snapshot.version != PANE_RUNTIME_PLUGIN_SNAPSHOT_VERSION {
            return Err(StatefulPluginError::UnsupportedVersion {
                plugin: PANE_RUNTIME_PLUGIN_SNAPSHOT_ID.as_str().to_string(),
                version: snapshot.version,
                expected: vec![PANE_RUNTIME_PLUGIN_SNAPSHOT_VERSION],
            });
        }
        let decoded: PaneRuntimeSnapshotV1 =
            serde_json::from_slice(&snapshot.bytes).map_err(|err| {
                StatefulPluginError::RestoreFailed {
                    plugin: PANE_RUNTIME_PLUGIN_SNAPSHOT_ID.as_str().to_string(),
                    details: err.to_string(),
                }
            })?;
        apply_pane_runtime_payload(&decoded);
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
    use bmux_attach_layout_protocol::PaneLaunchCommand;
    use bmux_pane_runtime_state::{
        AttachViewport, FloatingPaneLayer, FloatingPaneScope, PaneCommandSource,
    };
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
        let anchor_pane_id = Uuid::new_v4();
        let context_id = Uuid::new_v4();
        let client_id = Uuid::new_v4();
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
                    padding_override: Some(crate::padding::PanePaddingSpec {
                        left: 3,
                        max_content_width: Some(100),
                        horizontal_alignment: crate::padding::HorizontalAlignment::Center,
                        ..crate::padding::PanePaddingSpec::default()
                    }),
                }],
                focused_pane_id: Some(pane_id),
                zoomed_pane_id: Some(pane_id),
                layout_root: Some(PaneRuntimeSnapshotV1Layout::Split {
                    direction: PaneRuntimeSnapshotV1SplitDirection::Vertical,
                    ratio: 0.5,
                    first: Box::new(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                    second: Box::new(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                }),
                attach_viewport: Some(AttachViewport {
                    cols: 120,
                    rows: 40,
                    top_inset: 1,
                    right_inset: 0,
                    bottom_inset: 2,
                    left_inset: 0,
                }),
                floating_surfaces: vec![PaneRuntimeSnapshotV1FloatingSurface {
                    id: surface_id,
                    pane_id,
                    anchor_pane_id: Some(anchor_pane_id),
                    context_id: Some(context_id),
                    client_id: Some(client_id),
                    x: 1,
                    y: 2,
                    w: 40,
                    h: 10,
                    z: 0,
                    scope: FloatingPaneScope::PerWindow,
                    layer: FloatingPaneLayer::FloatingPane,
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
                    padding_override: None,
                }],
                focused_pane_id: Some(pane_id),
                zoomed_pane_id: None,
                layout_root: Some(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                attach_viewport: None,
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
                    padding_override: None,
                }],
                focused_pane_id: Some(pane_id),
                zoomed_pane_id: None,
                layout_root: Some(PaneRuntimeSnapshotV1Layout::Leaf { pane_id }),
                attach_viewport: None,
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

    /// `zoomed_pane_id` was added after v1 shipped, so it must be
    /// optional on decode: snapshots written by older builds have no
    /// such key and must restore as unzoomed rather than failing the
    /// whole pane-runtime section.
    #[test]
    fn schema_decodes_legacy_payload_without_zoomed_pane_id() {
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let legacy = serde_json::json!({
            "sessions": [{
                "session_id": session_id,
                "panes": [{
                    "id": pane_id,
                    "name": "pane-1",
                    "shell": "/bin/sh",
                }],
                "focused_pane_id": pane_id,
                "layout_root": { "kind": "leaf", "pane_id": pane_id },
            }]
        });
        let decoded: PaneRuntimeSnapshotV1 =
            serde_json::from_value(legacy).expect("legacy payload decodes without zoomed_pane_id");
        assert_eq!(
            decoded.sessions[0].zoomed_pane_id, None,
            "absent zoomed_pane_id restores as unzoomed"
        );
        assert_eq!(decoded.sessions[0].focused_pane_id, Some(pane_id));
        assert_eq!(decoded.sessions[0].panes[0].padding_override, None);
    }

    #[test]
    fn schema_omits_absent_padding_override_and_decodes_it_as_none() {
        let pane_id = Uuid::new_v4();
        let pane = PaneRuntimeSnapshotV1Pane {
            id: pane_id,
            name: None,
            shell: "/bin/sh".to_string(),
            launch_command: None,
            process_group_id: None,
            active_command: None,
            active_command_source: None,
            last_known_cwd: None,
            padding_override: None,
        };
        let value = serde_json::to_value(&pane).expect("encode pane snapshot");
        assert!(
            value
                .get("padding_override")
                .is_some_and(serde_json::Value::is_null),
            "an absent override must not be serialized as a live specification"
        );
        let decoded: PaneRuntimeSnapshotV1Pane =
            serde_json::from_value(value).expect("decode pane snapshot");
        assert_eq!(decoded.padding_override, None);
    }
}
