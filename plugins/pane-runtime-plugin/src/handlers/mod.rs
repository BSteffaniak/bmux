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
pub(crate) mod execution;
pub mod padding;
pub mod pane_commands;
pub mod pane_state;
mod spawn;

/// Route an inbound typed service call to the correct handler.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "`route_service!` centralizes all generated service route arms; splitting would obscure the macro dispatch table."
)]
pub fn route(context: NativeServiceContext) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "pane-runtime-presentation-wait-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::presentation_wait_capabilities())
        },
        "pane-runtime-presentation-wait-v1", "wait-present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::client::WaitPresentRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::wait_present(&req.execution, req.after.as_deref(), req.timeout_ms, req.max_bytes, || execution::authorize(ctx, &req.execution, "read")))
        },
        "pane-runtime-presentation-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(Ok::<_, bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionError>(bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::PresentationCapabilities { schema_version: 1, coherent_screen: true, canonical_resize: true }))
        },
        "pane-runtime-presentation-v1", "resize" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::ResizeRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "pane.resize").and_then(|()| crate::runtime::execution::resize_ordered(&req.execution, req.cols, req.rows)))
        },
        "pane-runtime-presentation-v1", "capture-events" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::CaptureEventsRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::capture_events(&req.execution, req.format, req.offset, req.max_bytes)))
        },
        "pane-runtime-presentation-v1", "present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::PresentRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::screen(&req.execution)))
        },
        "pane-runtime-execution-v4", "reserve" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ReserveRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::reservations::reserve(req.launch_mutation_id, req.options, |id| spawn::authorize_creation(ctx, id, "session.create").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") })))
        },
        "pane-runtime-execution-v4", "launch" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::LaunchRequest, ctx| {
            let context = ctx.clone();
            Ok::<_, ServiceResponse>(crate::runtime::reservations::launch(&req.reservation, move |id| spawn::authorize_creation(&context, id, "session.create").map_err(crate::runtime::spawn_failure)))
        },
        "pane-runtime-execution-v4", "reconcile" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ReconcileRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::reconcile(&req.execution)))
        },
        "pane-runtime-execution-v4", "send-admitted" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::SendAdmittedRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "pane.direct_input").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::send_admitted(&req.execution, req.input_mutation_id, req.data)))
        },
        "pane-runtime-execution-v4", "input-receipt" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::InputReceiptRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::input_receipt(&req.execution, req.input_mutation_id)))
        },
        "pane-runtime-execution-v4", "resolve" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ResolveRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "pane.close").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::resolve(&req.execution, req.resolution_mutation_id, req.expected_revision, req.action, req.input_mutation_id, req.grace_ms, ctx)))
        },
        "pane-runtime-execution-v4", "capture-resume" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::CaptureResumeRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.cursor.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::capture(req.cursor, req.max_bytes)))
        },
        "pane-runtime-execution-v4", "retire" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::RetireRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "pane.close").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::retire(&req.execution, req.retirement_mutation_id, req.expected_revision)))
        },
        "pane-runtime-execution-v4", "new-program-session" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::NewProgramSessionRequest, ctx| {
            let context = ctx.clone();
            Ok::<_, ServiceResponse>(crate::runtime::execution::create(req.name, req.options, move |id| {
                spawn::authorize_creation(&context, id, "session.create").map_err(crate::runtime::spawn_failure)
            }))
        },
        "pane-runtime-execution-v4", "status" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::StatusRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::status(&req.execution)))
        },
        "pane-runtime-execution-v4", "screen" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ScreenRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::screen(&req.execution)))
        },
        "pane-runtime-execution-v4", "send" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::SendRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "pane.direct_input").and_then(|()| crate::runtime::execution::send(&req.execution, req.data)))
        },
        "pane-runtime-execution-v4", "resize" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ResizeRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "pane.resize").and_then(|()| crate::runtime::execution::resize(&req.execution, req.cols, req.rows)))
        },
        "pane-runtime-execution-v4", "terminate" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::TerminateRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::terminate(&req.execution, req.grace_ms, || execution::authorize(ctx, &req.execution, "pane.close")))
        },
        "pane-runtime-execution-v4", "wait" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::WaitRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::wait(&req.execution, req.timeout_ms, || execution::authorize(ctx, &req.execution, "read")))
        },
        "pane-runtime-spawn-commands-v1", "new-session" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::NewSessionRequest, ctx| {
            let result = crate::runtime::new_session_v1(req, |session_id| {
                spawn::authorize_creation(ctx, session_id, "session.create").map_err(crate::runtime::spawn_failure)
            }).map(|session_id| bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionAck { session_id })
              .map_err(crate::runtime::spawn_wire_error);
            Ok::<_, ServiceResponse>(result)
        },
        "pane-runtime-spawn-commands-v1", "split-shell" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::SplitShellRequest, ctx| {
            let session_id = req.session_id;
            let result = crate::runtime::split_shell_v1(req, || {
                spawn::authorize_creation(ctx, session_id, "pane.split")
                    .map_err(crate::runtime::spawn_failure)
            }).map(|pane_id| bmux_pane_runtime_plugin_api::pane_runtime_commands::PaneAck { session_id, pane_id })
              .map_err(crate::runtime::spawn_wire_error);
            Ok::<_, ServiceResponse>(result)
        },
        "pane-runtime-spawn-v1", "home-directory" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::HomeDirectoryRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_read(ctx, req.session_id).and_then(|()| spawn::home_directory()))
        },
        "pane-runtime-spawn-v1", "capture-source" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::CaptureSourceRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_read(ctx, req.session_id).and_then(|()| crate::runtime::capture_spawn_source(req.session_id, req.pane_id)))
        },
        "pane-runtime-spawn-v1", "observe-directory" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::ObserveDirectoryRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_read(ctx, req.source.session_id).and_then(|()| crate::runtime::observe_spawn_directory(&req.source)))
        },
        // pane-runtime-state queries.
        "pane-runtime-state", "pane-padding" => |req: padding::PanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::get(&req))
        },
        "pane-runtime-state", "list-panes" => |req: pane_state::ListPanesArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_panes(&req, ctx))
        },
        "pane-runtime-state", "get-pane" => |req: pane_state::GetPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_state::get_pane(&req, ctx))
        },
        "pane-runtime-state", "list-floating-panes" => |req: pane_state::ListFloatingPanesArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_floating_panes(&req))
        },
        "pane-runtime-state", "list-pane-processes" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_pane_processes())
        },
        "pane-runtime-state", "get-pane-process" => |req: pane_state::GetPaneProcessArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_state::get_pane_process(&req))
        },

        // pane-runtime-commands mutations.
        "pane-runtime-commands", "set-pane-padding" => |req: padding::SetPanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::set(&req))
        },
        "pane-runtime-commands", "clear-pane-padding" => |req: padding::PanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::clear(&req))
        },
        "pane-runtime-commands", "begin-pane-padding-preview" => |req: padding::BeginPanePaddingPreviewArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::begin_preview(&req, ctx))
        },
        "pane-runtime-commands", "update-pane-padding-preview" => |req: padding::UpdatePanePaddingPreviewArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::update_preview(&req, ctx))
        },
        "pane-runtime-commands", "cancel-pane-padding-preview" => |req: padding::PreviewTokenArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::cancel_preview(&req, ctx))
        },
        "pane-runtime-commands", "commit-pane-padding-preview" => |req: padding::CommitPanePaddingPreviewArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::commit_preview(&req, ctx))
        },
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
        "pane-runtime-commands", "create-floating-pane" => |req: pane_commands::CreateFloatingPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::create_floating_pane(req, ctx))
        },
        "pane-runtime-commands", "move-floating-pane" => |req: pane_commands::MoveFloatingPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::move_floating_pane(&req, ctx))
        },
        "pane-runtime-commands", "resize-floating-pane" => |req: pane_commands::ResizeFloatingPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::resize_floating_pane(&req, ctx))
        },
        "pane-runtime-commands", "focus-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::focus_floating_pane(&req, ctx))
        },
        "pane-runtime-commands", "raise-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::raise_floating_pane(&req, ctx))
        },
        "pane-runtime-commands", "lower-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::lower_floating_pane(&req, ctx))
        },
        "pane-runtime-commands", "set-floating-pane-z" => |req: pane_commands::SetFloatingPaneZArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::set_floating_pane_z(&req, ctx))
        },
        "pane-runtime-commands", "set-floating-pane-layer" => |req: pane_commands::SetFloatingPaneLayerArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::set_floating_pane_layer(&req, ctx))
        },
        "pane-runtime-commands", "close-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::close_floating_pane(&req, ctx))
        },
        "pane-runtime-commands", "pane-direct-input" => |req: pane_commands::PaneDirectInputArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_direct_input(req, ctx))
        },
        "pane-runtime-commands", "execution-pane-input" => |req: pane_commands::ExecutionPaneInputArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::execution_pane_input(req))
        },
        "pane-runtime-commands", "destroy-execution-pane" => |req: pane_commands::ExecutionPaneTargetArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::destroy_execution_pane(&req))
        },
        "pane-runtime-commands", "ensure-execution-pane" => |req: pane_commands::EnsureExecutionPaneArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::ensure_execution_pane(req))
        },
        "pane-runtime-commands", "pane-set-pty-size" => |req: pane_commands::PaneSetPtySizeArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_set_pty_size(&req))
        },
        "pane-runtime-commands", "pane-signal" => |req: pane_commands::PaneSignalArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_signal(&req))
        },
        "pane-runtime-commands", "set-client-write-permission" => |req: pane_commands::SetClientWritePermissionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::set_client_write_permission(&req))
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
        "attach-runtime-commands", "suppress-pane-input-capture-v1" => |req: attach_commands::SuppressPaneInputCaptureArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::suppress_pane_input_capture(&req, ctx))
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
        "attach-runtime-state", "pane-output-cursor-state" => |req: attach_state::PaneOutputCursorArgs, _ctx| {
            Ok::<_, ServiceResponse>(attach_state::pane_output_cursor_state(&req))
        },
        "attach-runtime-state", "pane-grid-snapshot-state" => |req: attach_state::PaneGridSnapshotArgs, _ctx| {
            Ok::<_, ServiceResponse>(attach_state::pane_grid_snapshot_state(&req))
        },
        "attach-runtime-state", "attach-pane-grid-snapshot-state" => |req: attach_state::AttachPaneGridSnapshotArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_grid_snapshot_state(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-grid-window-state" => |req: attach_state::AttachPaneGridWindowArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_grid_window_state(&req, ctx))
        },
        "attach-runtime-state", "attach-history-capture-v1" => |req: attach_state::AttachPaneScrollbackPinArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_capture(&req, ctx))
        },
        "attach-runtime-state", "attach-main-row-slice-v1" => |req: attach_state::HistorySliceArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::main_row_slice(&req, ctx))
        },
        "attach-runtime-state", "attach-history-slice-v1" => |req: attach_state::HistorySliceArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_slice(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-scrollback-pin" => |req: attach_state::AttachPaneScrollbackPinArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_scrollback_pin(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-scrollback-unpin" => |req: attach_state::AttachPaneScrollbackUnpinArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_scrollback_unpin(&req, ctx))
        },
        "attach-runtime-state", "attach-pane-grid-delta-state" => |req: attach_state::AttachPaneGridDeltaArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_grid_delta_state(&req, ctx))
        },
        "attach-runtime-state", "attach-history-images-v1" => |req: attach_state::HistoryImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_images(&req, ctx))
        },
        "attach-runtime-state", "attach-history-images-v2" => |req: attach_state::HistoryImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_images_reflowed(&req, ctx))
        },
        "attach-runtime-state", "attach-history-images-v3" => |req: attach_state::AnchoredHistoryImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_images_anchored(&req, ctx))
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
                zoomed_pane_id: snapshot.zoomed_pane_id,
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
