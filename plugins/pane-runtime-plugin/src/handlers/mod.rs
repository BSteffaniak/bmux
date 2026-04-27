//! Typed-service request handlers for the pane-runtime plugin.
//!
//! Each BPDL interface has its own submodule; the top-level
//! `route` entry point dispatches via `bmux_plugin_sdk::route_service!`
//! into the submodule that owns each operation.
//!
//! Handlers reach pane-runtime state through the
//! `SessionRuntimeManagerHandle` registered in the plugin state
//! registry. When the handle is absent (e.g. during early boot or in
//! tests that didn't register a real manager), handlers return a
//! `handle_unavailable` error response.

use bmux_pane_runtime_plugin_api::pane_runtime_focus::{
    FocusSnapshot, STATE_KIND as FOCUS_STATE_KIND, SessionFocusStateMap,
};
use bmux_plugin_sdk::{NativeServiceContext, ServiceResponse};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub mod attach_commands;
pub mod attach_state;
pub mod pane_commands;
pub mod pane_state;

/// Route an inbound typed service call to the correct handler.
#[allow(
    clippy::needless_pass_by_value,
    reason = "`route_service!` macro requires the context to be owned so it can access `context.request.*` + `context.plugin_id` across match arms."
)]
pub fn route(context: NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        // pane-runtime-state queries.
        "pane-runtime-state", "list-panes" => |req: pane_state::ListPanesArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_panes(&req, ctx))
        },
        "pane-runtime-state", "get-pane" => |req: pane_state::GetPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_state::get_pane(&req, ctx))
        },
        "pane-runtime-state", "list-pane-processes" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_pane_processes())
        },
        "pane-runtime-state", "get-pane-process" => |req: pane_state::GetPaneProcessArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_state::get_pane_process(&req))
        },

        // pane-runtime-commands mutations.
        "pane-runtime-commands", "split-pane" => |req: pane_commands::SplitPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::split_pane(&req, ctx))
        },
        "pane-runtime-commands", "launch-pane" => |req: pane_commands::LaunchPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::launch_pane(req, ctx))
        },
        "pane-runtime-commands", "focus-pane" => |req: pane_commands::FocusPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::focus_pane(&req, ctx))
        },
        "pane-runtime-commands", "resize-pane" => |req: pane_commands::ResizePaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::resize_pane(&req, ctx))
        },
        "pane-runtime-commands", "close-pane" => |req: pane_commands::ClosePaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::close_pane(&req, ctx))
        },
        "pane-runtime-commands", "restart-pane" => |req: pane_commands::RestartPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::restart_pane(&req, ctx))
        },
        "pane-runtime-commands", "zoom-pane" => |req: pane_commands::ZoomPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::zoom_pane(&req, ctx))
        },
        "pane-runtime-commands", "pane-direct-input" => |req: pane_commands::PaneDirectInputArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_direct_input(req, ctx))
        },
        "pane-runtime-commands", "new-session-with-runtime" => |req: pane_commands::NewSessionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::new_session_with_runtime(&req))
        },
        "pane-runtime-commands", "kill-session-runtime" => |req: pane_commands::KillSessionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::kill_session_runtime(&req))
        },
        "pane-runtime-commands", "restore-session-runtime" => |_req: pane_commands::RestoreSessionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::restore_session_runtime())
        },

        // attach-runtime-commands.
        "attach-runtime-commands", "attach-session" => |req: attach_commands::AttachSessionArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_session(&req, ctx))
        },
        "attach-runtime-commands", "attach-context" => |req: attach_commands::AttachContextArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_context(&req, ctx))
        },
        "attach-runtime-commands", "attach-open" => |req: attach_commands::AttachOpenArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_open(&req, ctx))
        },
        "attach-runtime-commands", "attach-input" => |req: attach_commands::AttachInputArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_input(req, ctx))
        },
        "attach-runtime-commands", "attach-output" => |req: attach_commands::AttachOutputArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_output(&req, ctx))
        },
        "attach-runtime-commands", "attach-set-viewport" => |req: attach_commands::AttachSetViewportArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_set_viewport(&req, ctx))
        },
        "attach-runtime-commands", "attach-retarget-context" => |req: attach_commands::AttachRetargetContextArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_retarget_context(&req, ctx))
        },
        "attach-runtime-commands", "set-client-attach-policy" => |req: attach_commands::SetClientAttachPolicyArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::set_client_attach_policy(req, ctx))
        },
        "attach-runtime-commands", "detach" => |_req: attach_commands::DetachArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::detach(ctx))
        },

        // attach-runtime-state queries.
        "attach-runtime-state", "attach-layout-state" => |req: attach_state::AttachLayoutArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_layout_state(&req, ctx))
        },
        "attach-runtime-state", "attach-snapshot-state" => |req: attach_state::AttachSnapshotArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_snapshot_state(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-snapshot-state" => |req: attach_state::AttachPaneSnapshotArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_snapshot_state(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-output-batch" => |req: attach_state::AttachPaneOutputBatchArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_output_batch(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-images" => |req: attach_state::AttachPaneImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_images(&req, ctx))
        },
    })
}

/// Look up the `SessionRuntimeManagerHandle` from the plugin state
/// registry. Returns `None` when no manager is registered (the
/// corresponding handlers translate this into a `handle_unavailable`
/// error).
pub(super) fn session_runtime_handle()
-> Option<bmux_pane_runtime_state::SessionRuntimeManagerHandle> {
    bmux_plugin::global_plugin_state_registry()
        .get::<bmux_pane_runtime_state::SessionRuntimeManagerHandle>()
        .and_then(|arc| arc.read().ok().map(|g| (*g).clone()))
}

/// Monotonic counter for the focus-state channel. Incremented once
/// per [`publish_focus_state_snapshot`] call so subscribers can
/// deduplicate or order updates without relying on wall-clock time.
static FOCUS_STATE_REVISION: AtomicU64 = AtomicU64::new(0);

/// Publish the current per-session focus snapshot on the
/// `pane-runtime-focus` state channel.
///
/// Walks every live session via `list_session_ids` + `snapshot_session_runtime`
/// and builds a fresh `SessionFocusStateMap`. Called by pane-command
/// handlers after every mutation that could change focus (split,
/// focus, close, restart, zoom), and by `attach-open` so newly-
/// attached consumers observe the current state without round-trip.
/// Silently no-ops when the session-runtime manager handle isn't
/// registered (early boot, unit tests).
pub(crate) fn publish_focus_state_snapshot() {
    let Some(handle) = session_runtime_handle() else {
        return;
    };
    let mut entries = BTreeMap::new();
    for session_id in handle.0.list_session_ids() {
        let Some(snapshot) = handle.0.snapshot_session_runtime(session_id) else {
            continue;
        };
        entries.insert(
            session_id.0,
            FocusSnapshot {
                focused_pane_id: snapshot.focused_pane_id,
                // Zoom state isn't exposed on the snapshot API today;
                // a future trait method on `SessionRuntimeManagerApi`
                // could thread this through. Consumers treat `None`
                // as "not zoomed" and use `focused_pane_id` directly.
                zoomed_pane_id: None,
            },
        );
    }
    let revision = FOCUS_STATE_REVISION.fetch_add(1, Ordering::Relaxed) + 1;
    let payload = SessionFocusStateMap { entries, revision };
    // Ignore publish errors — the channel is registered in `activate`
    // before any mutation can run; a failure here means the plugin
    // was dropped, and there's nothing to do.
    let _ = bmux_plugin::global_event_bus().publish_state(&FOCUS_STATE_KIND, payload);
}
