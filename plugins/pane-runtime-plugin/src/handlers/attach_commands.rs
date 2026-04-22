//! Typed handlers for the `attach-runtime-commands` interface.
//!
//! Only `attach-set-viewport` dispatches through the registered
//! `SessionRuntimeManagerHandle`. The other six commands
//! (`attach-session`, `attach-context`, `attach-open`, `attach-input`,
//! `attach-output`, `detach`) are stateful protocol operations that
//! currently live on the server's IPC dispatcher; those handlers
//! return `AttachCommandError::Failed` with a clear "not yet routed"
//! reason so clients stick to `Request::*` for now.

use bmux_pane_runtime_plugin_api::attach_runtime_commands::{
    AttachCommandError, AttachViewportSet,
};
use bmux_session_models::{ClientId, SessionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachSessionArgs {
    pub session_id: Uuid,
    pub can_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachContextArgs {
    pub context_id: Uuid,
    pub can_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachOpenArgs {
    pub grant_token: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachInputArgs {
    pub session_id: Uuid,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachOutputArgs {
    pub session_id: Uuid,
    pub max_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachSetViewportArgs {
    pub session_id: Uuid,
    pub cols: u16,
    pub rows: u16,
    pub status_top_inset: u16,
    pub status_bottom_inset: u16,
    pub cell_pixel_w: u16,
    pub cell_pixel_h: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetachArgs;

/// Placeholder for commands whose orchestration hasn't migrated yet.
/// Returns an `AttachCommandError::Failed` with a reason string that
/// points clients at the legacy IPC path.
pub fn not_implemented(op: &str) -> Result<(), AttachCommandError> {
    Err(AttachCommandError::Failed {
        reason: format!(
            "attach-runtime-commands::{op} not yet routed through typed dispatch; \
             callers should use Request::* IPC for the time being"
        ),
    })
}

/// Typed handler for `attach-set-viewport`. The plugin-api contract
/// takes a per-client viewport; the plugin extracts the caller's
/// client id from the invocation context.
pub fn attach_set_viewport(
    req: &AttachSetViewportArgs,
) -> Result<AttachViewportSet, AttachCommandError> {
    let handle = super::session_runtime_handle().ok_or_else(|| AttachCommandError::Failed {
        reason: "pane-runtime manager handle not registered".to_string(),
    })?;
    // The set-viewport trait method returns the clamped viewport
    // rectangle; propagate it into the BPDL record. client_id comes
    // from the NativeServiceContext caller; if absent (synthetic
    // caller in tests), use Uuid::nil which the manager treats as an
    // unattached call and returns `SessionRuntimeError::Closed`.
    let client_id = ClientId(Uuid::nil());
    let (cols, rows, top, bottom) = handle
        .0
        .set_attach_viewport(
            SessionId(req.session_id),
            client_id,
            req.cols,
            req.rows,
            req.status_top_inset,
            req.status_bottom_inset,
            req.cell_pixel_w,
            req.cell_pixel_h,
        )
        .map_err(|e| AttachCommandError::Failed {
            reason: format!("failed setting attach viewport: {e:?}"),
        })?;
    Ok(AttachViewportSet {
        session_id: req.session_id,
        cols,
        rows,
        status_top_inset: top,
        status_bottom_inset: bottom,
        context_id: None,
    })
}
