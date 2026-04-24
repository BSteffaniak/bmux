//! Typed-client helpers for the `bmux.decoration` plugin.
//!
//! Free functions accepting any `C: TypedDispatchClient` that wrap
//! the plugin's BPDL service calls so callers don't have to repeat
//! interface / operation strings or serde boilerplate. The
//! capability, interface, and operation names all live here inside
//! the plugin-api crate; core code that wants to talk to the
//! decoration plugin depends on this crate and calls the helpers
//! directly, with no decoration-specific string constants leaking
//! out to other crates.
//!
//! Current helpers cover the client-side paths the attach runtime
//! uses today:
//!
//! - [`scene_snapshot`] — one-shot read of the current
//!   `DecorationScene`.
//! - [`notify_pane_geometry`] — push a pane's observed rect /
//!   content_rect so the plugin can paint against live geometry.
//! - [`forget_pane`] — evict the plugin's per-pane state when a
//!   pane disappears.
//!
//! All helpers route through
//! [`bmux_plugin_sdk::TypedDispatchClient::invoke_service_raw`] with
//! hardcoded capability/interface/op strings captured in private
//! constants at the top of this module. Errors are wrapped in
//! [`DecorationClientError`] so callers can disambiguate transport,
//! encode, and decode failures.

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use bmux_scene_protocol::scene_protocol::{DecorationScene, Rect};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::decoration_state::PaneGeometry;

const READ_CAPABILITY: &str = "bmux.decoration.read";
const WRITE_CAPABILITY: &str = "bmux.decoration.write";
const INTERFACE_DECORATION_STATE: &str = "decoration-state";
const OP_SCENE_SNAPSHOT: &str = "scene-snapshot";
const OP_NOTIFY_PANE_GEOMETRY: &str = "notify-pane-geometry";
const OP_FORGET_PANE: &str = "forget-pane";

/// Errors returned by decoration typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum DecorationClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode decoration request: {0}")]
    Encode(String),
    #[error("failed to decode decoration response: {0}")]
    Decode(String),
}

type Result<T> = core::result::Result<T, DecorationClientError>;

/// Fetch the decoration plugin's current scene.
///
/// Returns the decoded scene on success. The raw dispatcher error
/// surfaces on failure; callers that want to treat "plugin not
/// loaded" as a no-op should match on
/// [`DecorationClientError::Dispatch`] and degrade gracefully.
///
/// # Errors
///
/// Returns [`DecorationClientError::Encode`] / `Decode` for serde
/// failures or [`DecorationClientError::Dispatch`] when the
/// underlying `invoke_service_raw` call fails (including when the
/// plugin isn't loaded and the dispatcher reports the service as
/// unknown).
pub async fn scene_snapshot<C: TypedDispatchClient>(client: &mut C) -> Result<DecorationScene> {
    let payload =
        bmux_codec::to_vec(&()).map_err(|err| DecorationClientError::Encode(err.to_string()))?;
    let bytes = client
        .invoke_service_raw(
            READ_CAPABILITY,
            InvokeServiceKind::Query,
            INTERFACE_DECORATION_STATE,
            OP_SCENE_SNAPSHOT,
            payload,
        )
        .await?;
    bmux_codec::from_bytes(&bytes).map_err(|err| DecorationClientError::Decode(err.to_string()))
}

/// Push a pane's current rect + content-rect to the decoration
/// plugin via its typed `notify-pane-geometry` command.
///
/// # Errors
///
/// Returns [`DecorationClientError::Encode`] when serialising the
/// request fails or [`DecorationClientError::Dispatch`] on
/// transport errors. Callers that want "plugin absent → no-op"
/// semantics should log-and-continue on `Dispatch`.
pub async fn notify_pane_geometry<C: TypedDispatchClient>(
    client: &mut C,
    pane_id: Uuid,
    rect: Rect,
    content_rect: Rect,
) -> Result<()> {
    #[derive(Serialize)]
    struct Args {
        geometry: PaneGeometry,
    }
    let geometry = PaneGeometry {
        pane_id,
        rect,
        content_rect,
    };
    let payload = bmux_codec::to_vec(&Args { geometry })
        .map_err(|err| DecorationClientError::Encode(err.to_string()))?;
    client
        .invoke_service_raw(
            WRITE_CAPABILITY,
            InvokeServiceKind::Command,
            INTERFACE_DECORATION_STATE,
            OP_NOTIFY_PANE_GEOMETRY,
            payload,
        )
        .await?;
    Ok(())
}

/// Drop the plugin's per-pane state for `pane_id`. Called by the
/// attach runtime when a pane disappears from the observed layout.
///
/// # Errors
///
/// Same shape as [`notify_pane_geometry`].
pub async fn forget_pane<C: TypedDispatchClient>(client: &mut C, pane_id: Uuid) -> Result<()> {
    #[derive(Serialize, Deserialize)]
    struct Args {
        pane_id: Uuid,
    }
    let payload = bmux_codec::to_vec(&Args { pane_id })
        .map_err(|err| DecorationClientError::Encode(err.to_string()))?;
    client
        .invoke_service_raw(
            WRITE_CAPABILITY,
            InvokeServiceKind::Command,
            INTERFACE_DECORATION_STATE,
            OP_FORGET_PANE,
            payload,
        )
        .await?;
    Ok(())
}
