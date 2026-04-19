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

use bmux_codec::{from_bytes, to_vec};
use bmux_ipc::{InvokeServiceKind, PaneFocusDirection, PaneSplitDirection, SessionSelector};
use bmux_windows_plugin_api::windows_commands::{self, PaneAck, PaneDirection, Selector};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Capability guarding the windows plugin's mutating command surface.
pub const WINDOWS_WRITE_CAPABILITY: &str = "bmux.windows.write";

/// Interface id for the windows plugin's mutating command surface.
pub const WINDOWS_COMMANDS_INTERFACE: &str = "windows-commands";

/// BPDL-shaped typed arg structs for the `windows-commands` byte-wire
/// path. These mirror the [`bmux_windows_plugin_api::windows_commands`]
/// generated parameter lists exactly so the plugin's `route_service!`
/// decoder lands the same values the typed trait method would.
#[allow(dead_code)] // Not every arg struct is used by every caller.
pub mod args {
    use super::{PaneDirection, Selector};
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
        pub delta: i16,
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

/// Errors returned by [`invoke_with`].
#[derive(Debug)]
pub enum InvokeError {
    /// Serializing the typed arg struct to the wire format failed.
    Encode { operation: String, message: String },
    /// Deserializing the typed response from the wire format failed.
    Decode { operation: String, message: String },
    /// The client transport returned an error before the response was
    /// received.
    Client(String),
}

impl std::fmt::Display for InvokeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encode { operation, message } => {
                write!(f, "encoding {operation} args failed: {message}")
            }
            Self::Decode { operation, message } => {
                write!(f, "decoding {operation} response failed: {message}")
            }
            Self::Client(message) => write!(f, "client transport failed: {message}"),
        }
    }
}

impl std::error::Error for InvokeError {}

/// Minimal async abstraction over the two concrete IPC client types
/// ([`bmux_client::BmuxClient`] and
/// [`bmux_client::StreamingBmuxClient`]), both of which expose
/// `invoke_service_raw` with identical signatures but no shared trait.
///
/// Callers pass a closure that owns the client borrow and issues the
/// raw invocation; this keeps the helper client-type-agnostic.
#[allow(clippy::future_not_send)]
pub async fn invoke_with<F, Fut, Req, Resp>(
    operation: &str,
    args: &Req,
    invoke: F,
) -> Result<Resp, InvokeError>
where
    Req: Serialize + Sync,
    Resp: for<'de> Deserialize<'de>,
    F: FnOnce(Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let payload = to_vec(args).map_err(|source| InvokeError::Encode {
        operation: operation.to_string(),
        message: source.to_string(),
    })?;
    let response_bytes = invoke(payload).await.map_err(InvokeError::Client)?;
    from_bytes::<Resp>(&response_bytes).map_err(|source| InvokeError::Decode {
        operation: operation.to_string(),
        message: source.to_string(),
    })
}

/// Guard against drift between the hardcoded interface id strings and
/// the BPDL-generated constant. Keeping these in lock step is the
/// whole point of the typed contract.
const _: () = {
    const fn bytes_match(a: &str, b: &str) -> bool {
        if a.len() != b.len() {
            return false;
        }
        let a_bytes = a.as_bytes();
        let b_bytes = b.as_bytes();
        let mut i = 0;
        while i < a_bytes.len() {
            if a_bytes[i] != b_bytes[i] {
                return false;
            }
            i += 1;
        }
        true
    }
    assert!(bytes_match(
        WINDOWS_COMMANDS_INTERFACE,
        windows_commands::INTERFACE_ID,
    ));
};

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
