//! Typed attach-layout state-channel protocol.
//!
//! The attach runtime publishes an [`attach_layout_protocol::AttachLayoutSnapshot`]
//! whenever surfaces, visibility, or geometry change. Plugins that
//! consume layout state (decoration renderers, overlay managers) subscribe
//! via `EventBus::subscribe_state::<AttachLayoutSnapshot>` and see the
//! current snapshot on subscribe plus live updates as the layout shifts.
//!
//! The protocol is domain-agnostic: no decoration, overlay, or other
//! plugin is named. Each consumer decides how to react.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

/// Summary returned when listing panes in the active session runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneSummary {
    pub id: ::uuid::Uuid,
    pub index: u32,
    pub name: Option<String>,
    pub focused: bool,
    #[serde(default)]
    pub state: PaneState,
    #[serde(default)]
    pub state_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PaneState {
    #[default]
    Running,
    Exited,
}

bmux_plugin_schema_macros::schema! {
    source: "bpdl/attach-layout-protocol.bpdl",
    imports: {
        scene: {
            source: "../scene-protocol/bpdl/scene-protocol.bpdl",
            crate_path: ::bmux_scene_protocol,
        },
    },
}
