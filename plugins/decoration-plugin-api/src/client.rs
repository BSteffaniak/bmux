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
//! All helpers route through
//! [`bmux_plugin_sdk::TypedDispatchClient::invoke_service_raw`] with
//! hardcoded capability/interface/op strings captured in private
//! constants at the top of this module. Errors are wrapped in
//! [`DecorationClientError`] so callers can disambiguate transport,
//! encode, and decode failures.

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use bmux_scene_protocol::scene_protocol::DecorationScene;

const READ_CAPABILITY: &str = "bmux.decoration.read";
const INTERFACE_DECORATION_STATE: &str = "decoration-state";
const OP_SCENE_SNAPSHOT: &str = "scene-snapshot";

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
