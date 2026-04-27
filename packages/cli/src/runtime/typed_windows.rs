//! Typed helpers for dispatching `windows-commands` operations from
//! client-side code (attach runtime, playbook engine, etc.).
//!
//! The attach client process does not have a local plugin host, so it
//! cannot call the `WindowsCommandsService` trait methods directly.
//! These helpers route through the server's generic
//! `Request::InvokeService` envelope — the same path the plugin host
//! exposes for cross-plugin typed dispatch — so callers write typed
//! args and receive typed responses without hand-encoding IPC requests.
//!
//! Arg and response payloads are encoded with [`bmux_codec`], matching
//! the plugin's `route_service!` decoder byte-for-byte.

#![allow(dead_code)] // Pieces are consumed incrementally as call sites migrate.

use bmux_ipc::{InvokeServiceKind, PaneFocusDirection, PaneSplitDirection, SessionSelector};
use bmux_plugin_sdk::{CapabilityId, InterfaceId};
use bmux_windows_plugin_api::{
    capabilities::WINDOWS_WRITE,
    windows_commands::{self, PaneAck, PaneDirection, PaneResizeDirection, Selector},
};
use uuid::Uuid;

/// Capability guarding the windows plugin's mutating command surface.
///
/// Re-exported from [`bmux_windows_plugin_api::capabilities::WINDOWS_WRITE`]
/// so callers here and in the attach runtime consume the same typed
/// constant the plugin author declared.
pub const WINDOWS_WRITE_CAPABILITY: CapabilityId = WINDOWS_WRITE;

/// Interface id for the windows plugin's mutating command surface.
///
/// Re-exported from the BPDL-generated
/// [`bmux_windows_plugin_api::windows_commands::INTERFACE_ID`] so every
/// caller in core or CLI-side code reaches the same typed constant
/// rather than hand-typing the string.
pub const WINDOWS_COMMANDS_INTERFACE: InterfaceId = windows_commands::INTERFACE_ID;

/// BPDL-shaped typed arg structs for the `windows-commands` byte-wire
/// path. These mirror the [`bmux_windows_plugin_api::windows_commands`]
/// generated parameter lists exactly so the plugin's `route_service!`
/// decoder lands the same values the typed trait method would.
#[allow(dead_code)] // Not every arg struct is used by every caller.
pub mod args {
    use super::{PaneDirection, PaneResizeDirection, Selector};
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FocusPane {
        pub id: Uuid,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ClosePane {
        pub id: Uuid,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SplitPane {
        #[serde(default)]
        pub session: Option<Selector>,
        #[serde(default)]
        pub target: Option<Selector>,
        pub direction: PaneDirection,
        #[serde(default)]
        pub ratio_pct: Option<u32>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LaunchPane {
        #[serde(default)]
        pub session: Option<Selector>,
        #[serde(default)]
        pub target: Option<Selector>,
        pub direction: PaneDirection,
        #[serde(default)]
        pub name: Option<String>,
        pub program: String,
        #[serde(default)]
        pub args: Vec<String>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ResizePane {
        #[serde(default)]
        pub session: Option<Selector>,
        #[serde(default)]
        pub target: Option<Selector>,
        pub direction: PaneResizeDirection,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ZoomPane {
        #[serde(default)]
        pub session: Option<Selector>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FocusPaneBySelector {
        #[serde(default)]
        pub session: Option<Selector>,
        pub target: Selector,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ClosePaneBySelector {
        #[serde(default)]
        pub session: Option<Selector>,
        pub target: Selector,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CloseActivePane {
        #[serde(default)]
        pub session: Option<Selector>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct FocusPaneInDirection {
        #[serde(default)]
        pub session: Option<Selector>,
        pub direction: PaneDirection,
    }
}

/// Convert an IPC [`SessionSelector`] to the typed BPDL [`Selector`].
#[must_use]
pub fn ipc_to_typed_selector(selector: SessionSelector) -> Selector {
    match selector {
        SessionSelector::ById(id) => Selector {
            id: Some(id),
            name: None,
            index: None,
        },
        SessionSelector::ByName(name) => Selector {
            id: None,
            name: Some(name),
            index: None,
        },
    }
}

/// Convert an IPC [`bmux_ipc::PaneSelector`] to the typed BPDL
/// [`Selector`]. `Active` cannot be expressed in the typed schema and
/// folds to an empty selector (the plugin interprets "no selector
/// fields" as "active pane").
#[must_use]
pub const fn ipc_pane_to_typed_selector(selector: &bmux_ipc::PaneSelector) -> Selector {
    match selector {
        bmux_ipc::PaneSelector::ById(id) => Selector {
            id: Some(*id),
            name: None,
            index: None,
        },
        bmux_ipc::PaneSelector::ByIndex(index) => Selector {
            id: None,
            name: None,
            index: Some(*index),
        },
        bmux_ipc::PaneSelector::Active => Selector {
            id: None,
            name: None,
            index: None,
        },
    }
}

/// Convert an IPC [`PaneFocusDirection`] to the typed BPDL
/// [`PaneDirection`] used by pane commands.
#[must_use]
pub const fn ipc_focus_to_typed_direction(direction: PaneFocusDirection) -> PaneDirection {
    match direction {
        PaneFocusDirection::Next => PaneDirection::Right,
        PaneFocusDirection::Prev => PaneDirection::Left,
    }
}

/// Convert an IPC [`PaneSplitDirection`] to the typed BPDL
/// [`PaneDirection`].
#[must_use]
pub const fn ipc_split_to_typed_direction(direction: PaneSplitDirection) -> PaneDirection {
    match direction {
        PaneSplitDirection::Vertical => PaneDirection::Vertical,
        PaneSplitDirection::Horizontal => PaneDirection::Horizontal,
    }
}

/// Re-export of [`bmux_windows_plugin_api::windows_commands::PaneAck`]
/// so callers can deserialize responses without pulling the api crate
/// in directly.
pub type PaneAckResponse = PaneAck;

/// Re-export of [`bmux_ipc::InvokeServiceKind`] for the typed command
/// dispatch path, which always uses the Command kind.
pub const COMMAND_KIND: InvokeServiceKind = InvokeServiceKind::Command;

/// Construct a typed [`Selector`] addressing a pane by UUID.
#[must_use]
pub const fn pane_selector_by_id(id: Uuid) -> Selector {
    Selector {
        id: Some(id),
        name: None,
        index: None,
    }
}
