//! Typed-client helpers for the `bmux.pane_runtime` plugin.
//!
//! Free functions accepting any `C: TypedDispatchClient` that wrap
//! the plugin's BPDL service calls so callers don't have to repeat
//! interface/operation strings + serde boilerplate.
//!
//! The helpers come in four families matching the four BPDL
//! interfaces:
//!
//! - `pane-runtime-state` queries (`list_panes`, `get_pane`).
//! - `pane-runtime-commands` mutations (`split_pane`, `launch_pane`,
//!   `focus_pane`, `resize_pane`, `close_pane`, `restart_pane`,
//!   `zoom_pane`, `pane_direct_input`, `new_session_with_runtime`,
//!   `kill_session_runtime`, `restore_session_runtime`).
//! - `attach-runtime-commands` (`attach_session`, `attach_context`,
//!   `attach_open`, `attach_input`, `attach_output`,
//!   `attach_set_viewport`, `detach`).
//! - `attach-runtime-state` (`attach_layout_state`,
//!   `attach_snapshot_state`, `attach_pane_snapshot_state`,
//!   `attach_pane_output_batch`, `attach_pane_images`).

use bmux_ipc::InvokeServiceKind;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capabilities::{
    ATTACH_RUNTIME_READ, ATTACH_RUNTIME_WRITE, PANE_RUNTIME_READ, PANE_RUNTIME_WRITE,
};
use crate::{
    attach_runtime_commands, attach_runtime_state, pane_runtime_commands, pane_runtime_state,
};

/// Errors returned by pane-runtime typed-client helpers.
#[derive(Debug, thiserror::Error)]
pub enum PaneRuntimeTypedClientError {
    #[error(transparent)]
    Dispatch(#[from] TypedDispatchClientError),
    #[error("failed to encode pane-runtime request: {0}")]
    Encode(String),
    #[error("failed to decode pane-runtime response: {0}")]
    Decode(String),
}

type Result<T> = core::result::Result<T, PaneRuntimeTypedClientError>;

async fn invoke<C, Req, Resp>(
    client: &mut C,
    capability: &str,
    kind: InvokeServiceKind,
    interface: &str,
    operation: &str,
    request: &Req,
) -> Result<Resp>
where
    C: TypedDispatchClient,
    Req: Serialize,
    Resp: serde::de::DeserializeOwned,
{
    let payload = bmux_ipc::encode(request)
        .map_err(|e| PaneRuntimeTypedClientError::Encode(e.to_string()))?;
    let response_bytes = client
        .invoke_service_raw(capability, kind, interface, operation, payload)
        .await?;
    bmux_ipc::decode(&response_bytes)
        .map_err(|e| PaneRuntimeTypedClientError::Decode(e.to_string()))
}

// ── pane-runtime-state queries ───────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ListPanesArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetPaneArgs {
    session_id: Uuid,
    pane_id: Uuid,
}

/// List the panes in a session runtime. When `session_id` is `None`
/// the server resolves the caller's currently-selected session.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn list_panes<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Option<Uuid>,
) -> Result<
    core::result::Result<pane_runtime_state::SessionPaneList, pane_runtime_state::PaneStateError>,
> {
    invoke(
        client,
        PANE_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        pane_runtime_state::INTERFACE_ID.as_str(),
        "list-panes",
        &ListPanesArgs { session_id },
    )
    .await
}

/// Fetch a single pane's summary.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn get_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    pane_id: Uuid,
) -> Result<core::result::Result<pane_runtime_state::PaneSummary, pane_runtime_state::PaneStateError>>
{
    invoke(
        client,
        PANE_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        pane_runtime_state::INTERFACE_ID.as_str(),
        "get-pane",
        &GetPaneArgs {
            session_id,
            pane_id,
        },
    )
    .await
}

// ── pane-runtime-commands mutations ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SplitPaneArgs {
    session_id: Uuid,
    #[serde(default)]
    target: Option<Uuid>,
    direction: String,
    ratio_percent: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LaunchPaneArgs {
    session_id: Uuid,
    #[serde(default)]
    target: Option<Uuid>,
    direction: String,
    ratio_percent: u8,
    #[serde(default)]
    name: Option<String>,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FocusPaneArgs {
    session_id: Uuid,
    #[serde(default)]
    target: Option<Uuid>,
    direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResizePaneArgs {
    session_id: Uuid,
    #[serde(default)]
    target: Option<Uuid>,
    direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TargetedPaneArgs {
    session_id: Uuid,
    #[serde(default)]
    target: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ZoomPaneArgs {
    session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaneDirectInputArgs {
    session_id: Uuid,
    pane_id: Uuid,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NewSessionArgs {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KillSessionArgs {
    session_id: Uuid,
    force_local: bool,
}

/// Split an existing pane.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn split_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    target: Option<Uuid>,
    direction: &str,
    ratio_percent: u8,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "split-pane",
        &SplitPaneArgs {
            session_id,
            target,
            direction: direction.to_string(),
            ratio_percent,
        },
    )
    .await
}

/// Launch a new pane with an explicit program.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn launch_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    target: Option<Uuid>,
    direction: &str,
    ratio_percent: u8,
    name: Option<String>,
    program: String,
    args: Vec<String>,
    cwd: Option<String>,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "launch-pane",
        &LaunchPaneArgs {
            session_id,
            target,
            direction: direction.to_string(),
            ratio_percent,
            name,
            program,
            args,
            cwd,
        },
    )
    .await
}

/// Focus a pane by id or direction.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn focus_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    target: Option<Uuid>,
    direction: &str,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "focus-pane",
        &FocusPaneArgs {
            session_id,
            target,
            direction: direction.to_string(),
        },
    )
    .await
}

/// Resize a pane.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn resize_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    target: Option<Uuid>,
    direction: &str,
) -> Result<
    core::result::Result<
        pane_runtime_commands::SessionAck,
        pane_runtime_commands::PaneCommandError,
    >,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "resize-pane",
        &ResizePaneArgs {
            session_id,
            target,
            direction: direction.to_string(),
        },
    )
    .await
}

/// Close a pane (and its session when it was the last pane).
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn close_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    target: Option<Uuid>,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "close-pane",
        &TargetedPaneArgs { session_id, target },
    )
    .await
}

/// Restart a pane's process.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn restart_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    target: Option<Uuid>,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "restart-pane",
        &TargetedPaneArgs { session_id, target },
    )
    .await
}

/// Toggle pane zoom.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn zoom_pane<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "zoom-pane",
        &ZoomPaneArgs { session_id },
    )
    .await
}

/// Write raw bytes directly to a pane (bypasses attach).
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn pane_direct_input<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    pane_id: Uuid,
    data: Vec<u8>,
) -> Result<
    core::result::Result<pane_runtime_commands::PaneAck, pane_runtime_commands::PaneCommandError>,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "pane-direct-input",
        &PaneDirectInputArgs {
            session_id,
            pane_id,
            data,
        },
    )
    .await
}

/// Spin up a new session runtime.
///
/// Today the server still owns session-creation orchestration; the
/// plugin-side handler returns `SessionRuntimeCommandError::Failed`
/// until that migration lands. Callers are expected to use
/// `Request::NewSession` for the time being.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn new_session_with_runtime<C: TypedDispatchClient>(
    client: &mut C,
    name: Option<String>,
) -> Result<
    core::result::Result<
        pane_runtime_commands::SessionAck,
        pane_runtime_commands::SessionRuntimeCommandError,
    >,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "new-session-with-runtime",
        &NewSessionArgs { name },
    )
    .await
}

/// Tear down a session's pane runtime.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn kill_session_runtime<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    force_local: bool,
) -> Result<
    core::result::Result<
        pane_runtime_commands::SessionAck,
        pane_runtime_commands::SessionRuntimeCommandError,
    >,
> {
    invoke(
        client,
        PANE_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        pane_runtime_commands::INTERFACE_ID.as_str(),
        "kill-session-runtime",
        &KillSessionArgs {
            session_id,
            force_local,
        },
    )
    .await
}

// ── attach-runtime-commands ──────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachSessionArgs {
    selector: bmux_ipc::SessionSelector,
    can_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachContextArgs {
    selector: bmux_ipc::ContextSelector,
    can_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachOpenArgs {
    session_id: Uuid,
    attach_token: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachInputArgs {
    session_id: Uuid,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachOutputArgs {
    session_id: Uuid,
    max_bytes: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SetClientAttachPolicyArgs {
    allow_detach: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
struct DetachArgs;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachSetViewportArgs {
    session_id: Uuid,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachRetargetContextArgs {
    context_id: Uuid,
    can_write: bool,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
}

/// Request an attach grant for a session.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_session<C: TypedDispatchClient>(
    client: &mut C,
    selector: bmux_ipc::SessionSelector,
    can_write: bool,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachGrant,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-session",
        &AttachSessionArgs {
            selector,
            can_write,
        },
    )
    .await
}

/// Request an attach grant for a context.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_context<C: TypedDispatchClient>(
    client: &mut C,
    selector: bmux_ipc::ContextSelector,
    can_write: bool,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachGrant,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-context",
        &AttachContextArgs {
            selector,
            can_write,
        },
    )
    .await
}

/// Open an attach stream using a previously-issued grant token.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_open<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    attach_token: Uuid,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachReady,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-open",
        &AttachOpenArgs {
            session_id,
            attach_token,
        },
    )
    .await
}

/// Write attach input bytes into a session's active runtime.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_input<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    data: Vec<u8>,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachInputAccepted,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-input",
        &AttachInputArgs { session_id, data },
    )
    .await
}

/// Drain attach output for the calling client.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_output<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    max_bytes: u32,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachOutput,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-output",
        &AttachOutputArgs {
            session_id,
            max_bytes,
        },
    )
    .await
}

/// Update the calling client's attach policy (whether server may detach it).
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn set_client_attach_policy<C: TypedDispatchClient>(
    client: &mut C,
    allow_detach: bool,
) -> Result<core::result::Result<u8, attach_runtime_commands::AttachCommandError>> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "set-client-attach-policy",
        &SetClientAttachPolicyArgs { allow_detach },
    )
    .await
}

/// Detach the calling client from its current attach (if any).
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn detach<C: TypedDispatchClient>(
    client: &mut C,
) -> Result<core::result::Result<u8, attach_runtime_commands::AttachCommandError>> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "detach",
        &DetachArgs,
    )
    .await
}

/// Publish an updated client viewport to the pane runtime.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn attach_set_viewport<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachViewportSet,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-set-viewport",
        &AttachSetViewportArgs {
            session_id,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_w,
            cell_pixel_h,
        },
    )
    .await
}

/// Retarget the calling attach client to a context and update its viewport.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn attach_retarget_context<C: TypedDispatchClient>(
    client: &mut C,
    context_id: Uuid,
    can_write: bool,
    cols: u16,
    rows: u16,
    status_top_inset: u16,
    status_bottom_inset: u16,
    cell_pixel_w: u16,
    cell_pixel_h: u16,
) -> Result<
    core::result::Result<
        attach_runtime_commands::AttachRetargetReady,
        attach_runtime_commands::AttachCommandError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_WRITE.as_str(),
        InvokeServiceKind::Command,
        attach_runtime_commands::INTERFACE_ID.as_str(),
        "attach-retarget-context",
        &AttachRetargetContextArgs {
            context_id,
            can_write,
            cols,
            rows,
            status_top_inset,
            status_bottom_inset,
            cell_pixel_w,
            cell_pixel_h,
        },
    )
    .await
}

// ── attach-runtime-state queries ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachLayoutArgs {
    session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachSnapshotArgs {
    session_id: Uuid,
    max_bytes_per_pane: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachPaneSnapshotArgs {
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes_per_pane: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachPaneOutputBatchArgs {
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes: u32,
}

/// Fetch an attach-layout snapshot for the calling client.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_layout_state<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
) -> Result<
    core::result::Result<
        attach_runtime_state::AttachLayout,
        attach_runtime_state::AttachStateError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        attach_runtime_state::INTERFACE_ID.as_str(),
        "attach-layout-state",
        &AttachLayoutArgs { session_id },
    )
    .await
}

/// Fetch a full attach snapshot (layout + per-pane content) for the
/// calling client.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_snapshot_state<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    max_bytes_per_pane: u32,
) -> Result<
    core::result::Result<
        attach_runtime_state::AttachSnapshot,
        attach_runtime_state::AttachStateError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        attach_runtime_state::INTERFACE_ID.as_str(),
        "attach-snapshot-state",
        &AttachSnapshotArgs {
            session_id,
            max_bytes_per_pane,
        },
    )
    .await
}

/// Fetch a per-pane snapshot for the calling client.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_pane_snapshot_state<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes_per_pane: u32,
) -> Result<
    core::result::Result<
        attach_runtime_state::AttachPaneSnapshot,
        attach_runtime_state::AttachStateError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        attach_runtime_state::INTERFACE_ID.as_str(),
        "attach-pane-snapshot-state",
        &AttachPaneSnapshotArgs {
            session_id,
            pane_ids,
            max_bytes_per_pane,
        },
    )
    .await
}

/// Drain per-pane output for the calling client.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_pane_output_batch<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    max_bytes: u32,
) -> Result<
    core::result::Result<
        attach_runtime_state::AttachPaneOutputBatch,
        attach_runtime_state::AttachStateError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        attach_runtime_state::INTERFACE_ID.as_str(),
        "attach-pane-output-batch",
        &AttachPaneOutputBatchArgs {
            session_id,
            pane_ids,
            max_bytes,
        },
    )
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachPaneImagesArgs {
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    since_sequences: Vec<u64>,
}

/// Fetch per-pane image deltas for the calling client.
///
/// # Errors
///
/// Returns an error if transport, encoding, or server-side operation fails.
pub async fn attach_pane_images<C: TypedDispatchClient>(
    client: &mut C,
    session_id: Uuid,
    pane_ids: Vec<Uuid>,
    since_sequences: Vec<u64>,
) -> Result<
    core::result::Result<
        attach_runtime_state::AttachPaneImages,
        attach_runtime_state::AttachStateError,
    >,
> {
    invoke(
        client,
        ATTACH_RUNTIME_READ.as_str(),
        InvokeServiceKind::Query,
        attach_runtime_state::INTERFACE_ID.as_str(),
        "attach-pane-images",
        &AttachPaneImagesArgs {
            session_id,
            pane_ids,
            since_sequences,
        },
    )
    .await
}
