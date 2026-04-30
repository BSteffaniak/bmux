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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AttachLayer {
    Status,
    Pane,
    Overlay,
    FloatingPane,
    Tooltip,
    Cursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachFocusTarget {
    None,
    Pane { pane_id: ::uuid::Uuid },
    Surface { surface_id: ::uuid::Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachSurfaceKind {
    Pane,
    FloatingPane,
    Modal,
    Overlay,
    Tooltip,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Wire DTO mirrors independent surface flags.
pub struct AttachSurface {
    pub id: ::uuid::Uuid,
    pub kind: AttachSurfaceKind,
    pub layer: AttachLayer,
    pub z: i32,
    pub rect: AttachRect,
    /// Region within `rect` where the pane's PTY content lives. Authoritative;
    /// consumers (renderer, PTY sizer, mouse hit-tester, image compositor) must
    /// read this rather than subtracting borders from `rect`. Scene producers
    /// fill this in based on whatever decoration is applied; when no
    /// decoration is present, `content_rect == rect`.
    pub content_rect: AttachRect,
    /// Named sub-rectangles of `rect` that belong to a plugin-owned surface
    /// decoration. Hit-testing routes clicks on these regions to the owning
    /// plugin as `SurfaceRegionMouseEvent`s. Must not overlap `content_rect`
    /// or each other.
    #[serde(default)]
    pub interactive_regions: Vec<InteractiveRegion>,
    pub opaque: bool,
    pub visible: bool,
    pub accepts_input: bool,
    pub cursor_owner: bool,
    pub pane_id: Option<::uuid::Uuid>,
}

/// Named rectangular region within an `AttachSurface` that routes mouse
/// input to a plugin. The region is rendered by the plugin that declared
/// it; core only hit-tests and dispatches events.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InteractiveRegion {
    /// Absolute terminal coordinates of this region.
    pub rect: AttachRect,
    /// Plugin-chosen identifier, unique within the owning surface.
    pub region_id: String,
    /// Plugin id that owns this region. Mouse events on this region are
    /// dispatched to this plugin.
    pub owning_plugin_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachScene {
    pub session_id: ::uuid::Uuid,
    pub focus: AttachFocusTarget,
    pub surfaces: Vec<AttachSurface>,
}

/// Pane selector accepted by pane-runtime commands and protocol helpers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PaneSelector {
    ById(::uuid::Uuid),
    ByIndex(u32),
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSplitDirection {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PaneLaunchCommand {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: ::std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneFocusDirection {
    Next,
    Prev,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneLayoutNode {
    Leaf {
        pane_id: ::uuid::Uuid,
    },
    Split {
        direction: PaneSplitDirection,
        ratio_percent: u8,
        first: Box<Self>,
        second: Box<Self>,
    },
}

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
