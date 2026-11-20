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

pub mod attach_commands;
pub mod attach_state;
pub(crate) mod execution;
pub(crate) mod human_input;
pub(crate) mod live_presentation;
pub(crate) mod output_push;
pub mod padding;
pub mod pane_commands;
pub mod pane_state;
mod pointer_grid;
mod spawn;

/// Route an inbound typed service call to the correct handler.
#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "`route_service!` centralizes all generated service route arms; splitting would obscure the macro dispatch table."
)]
pub fn route(
    host: &crate::runtime::PaneRuntimeHost,
    context: &NativeServiceContext,
) -> ServiceResponse {
    if context.caller_client_id.is_some()
        && context.request.service.interface_id == "pane-runtime-readiness-v1"
        && matches!(
            context.request.operation.as_str(),
            "configure-managed-idle" | "fence-managed-idle"
        )
    {
        return ServiceResponse::error(
            "managed_lifecycle_owner_required",
            "managed retirement is owned by the local server launch policy",
        );
    }
    if let Some(client) = context.caller_client_id
        && !attach_commands::observer_operation_allowed(
            host,
            bmux_session_models::ClientId(client),
            &context.request.service.interface_id,
            &context.request.operation,
        )
    {
        return ServiceResponse::error(
            "observer_read_only",
            "observer connection cannot mutate pane runtime",
        );
    }
    if let Err(error) = execution::authorize_route(context) {
        return error;
    }
    match crate::runtime::input_authority::with_local_settled(host, context.clone(), || {
        route_authorized(host, context)
    }) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("input_settlement_failed", &error),
    }
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "Generated routes are kept together as the explicit service dispatch table."
)]
fn route_authorized(
    host: &crate::runtime::PaneRuntimeHost,
    context: &NativeServiceContext,
) -> ServiceResponse {
    bmux_plugin_sdk::route_service!(context, {
        "pane-runtime-environment-v1", "capabilities" => |_req: (), _ctx| { Ok::<_, ServiceResponse>(2_u32) },
        "pane-runtime-environment-v1", "reserve" => |req: bmux_pane_runtime_plugin_api::pane_runtime_environment_v1::client::ReserveRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::reservations::reserve_environment(host, req.launch_mutation_id, req.options, req.environment, |id| spawn::authorize_creation(ctx, id, "session.create").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") })))
        },
        "pane-runtime-pointer-grid-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::GridCapabilities { format_versions: vec![1] })
        },
        "pane-runtime-pointer-grid-v1", "snapshots" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::SnapshotsRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(attach_state::attach_pane_grid_snapshot_state(host, &attach_state::AttachPaneGridSnapshotArgs { session_id: req.session_id, pane_ids: req.pane_ids, max_rows_per_pane: req.max_rows_per_pane }, ctx))
        },
        "pane-runtime-pointer-grid-v1", "snapshot" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::SnapshotRequest, _ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(attach_state::pane_grid_snapshot_state(host, &attach_state::PaneGridSnapshotArgs { session_id: req.session_id, pane_id: req.pane_id, max_rows: req.max_rows }))
        },
        "pane-runtime-pointer-grid-v1", "frames" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::FramesRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(attach_state::attach_pane_frames(host, &attach_state::AttachPaneFramesArgs { session_id: req.session_id, pane_ids: req.pane_ids, max_history_lines: req.max_history_lines }, ctx))
        },
        "pane-runtime-pointer-grid-v1", "windows" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::WindowsRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(attach_state::attach_pane_grid_window_state(host, &attach_state::AttachPaneGridWindowArgs { session_id: req.session_id, windows: req.windows }, ctx))
        },
        "pane-runtime-pointer-grid-v1", "screen" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::ScreenRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::screen(host, &req.execution)))
        },
        "pane-runtime-pointer-grid-v1", "wait-present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::WaitPresentRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(crate::runtime::execution::wait_present(host, &req.execution, req.after.as_deref(), req.timeout_ms, req.max_bytes, || execution::authorize(ctx, &req.execution, "read")))
        },
        "pane-runtime-pointer-grid-v1", "live-read" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::LiveReadRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(live_presentation::read(host, ctx, &req.execution, req.after.as_ref(), req.max_bytes))
        },
        "pane-runtime-pointer-grid-v1", "live-wait-present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_pointer_grid_v1::client::LiveWaitPresentRequest, ctx| {
            pointer_grid::select(req.format_version)?;
            Ok::<_, ServiceResponse>(live_presentation::wait_present(host, ctx, &req.execution, req.after.as_ref(), req.timeout_ms, req.max_bytes))
        },
        "pane-runtime-live-presentation-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(live_presentation::capabilities())
        },
        "pane-runtime-live-presentation-v1", "read" => |req: bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::client::ReadRequest, ctx| {
            pointer_grid::legacy(live_presentation::read(host, ctx, &req.execution, req.after.as_ref(), req.max_bytes))
        },
        "pane-runtime-live-presentation-v1", "wait-present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_live_presentation_v1::client::WaitPresentRequest, ctx| {
            pointer_grid::legacy(live_presentation::wait_present(host, ctx, &req.execution, req.after.as_ref(), req.timeout_ms, req.max_bytes))
        },
        "pane-runtime-presentation-wait-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::presentation_wait_capabilities())
        },
        "pane-runtime-presentation-wait-v1", "wait-present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_wait_v1::client::WaitPresentRequest, ctx| {
            pointer_grid::legacy(crate::runtime::execution::wait_present(host, &req.execution, req.after.as_deref(), req.timeout_ms, req.max_bytes, || execution::authorize(ctx, &req.execution, "read")))
        },
        "pane-runtime-view-size-v1", "resize-alternate" => |req: bmux_pane_runtime_plugin_api::pane_runtime_view_size_v1::client::ResizeAlternateRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::resize_alternate(host, &req.execution, req.expected_grid_revision, req.cols, req.rows, || execution::authorize(ctx, &req.execution, "pane.resize")))
        },
        "pane-runtime-presentation-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(Ok::<_, bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionError>(bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::PresentationCapabilities { schema_version: 1, coherent_screen: true, canonical_resize: true }))
        },
        "pane-runtime-presentation-v1", "resize" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::ResizeRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "pane.resize").and_then(|()| crate::runtime::execution::resize_ordered(host, &req.execution, req.cols, req.rows)))
        },
        "pane-runtime-presentation-v1", "capture-events" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::CaptureEventsRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::capture_events(host, &req.execution, req.format, req.offset, req.max_bytes)))
        },
        "pane-runtime-presentation-v1", "present" => |req: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::client::PresentRequest, ctx| {
            pointer_grid::legacy(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::screen(host, &req.execution)))
        },
        "pane-runtime-execution-v4", "reserve" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ReserveRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::reservations::reserve(host, req.launch_mutation_id, req.options, |id| spawn::authorize_creation(ctx, id, "session.create").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") })))
        },
        "pane-runtime-execution-v4", "launch" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::LaunchRequest, ctx| {
            let context = ctx.clone();
            Ok::<_, ServiceResponse>(crate::runtime::reservations::launch(host, &req.reservation, move |id| spawn::authorize_creation(&context, id, "session.create").map_err(crate::runtime::spawn_failure)))
        },
        "pane-runtime-execution-v4", "reconcile" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ReconcileRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::reconcile(host, &req.execution)))
        },
        "pane-runtime-execution-v4", "send-admitted" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::SendAdmittedRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "pane.direct_input").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::send_admitted(host, &req.execution, req.input_mutation_id, req.data, execution::input_authority(ctx, &req.execution).map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") })?)))
        },
        "pane-runtime-execution-v4", "input-receipt" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::InputReceiptRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::input_receipt(host, &req.execution, req.input_mutation_id)))
        },
        "pane-runtime-execution-v4", "resolve" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ResolveRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "pane.close").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::resolve(host, &req.execution, req.resolution_mutation_id, req.expected_revision, req.action, req.input_mutation_id, req.grace_ms, ctx)))
        },
        "pane-runtime-execution-v4", "capture-resume" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::CaptureResumeRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.cursor.execution.session_id, "read").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::capture(host, req.cursor, req.max_bytes)))
        },
        "pane-runtime-execution-v4", "retire" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::RetireRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_session(ctx, req.execution.session_id, "pane.close").map_err(|error| bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRecoveryError::Denied { reason: format!("{error:?}") }).and_then(|()| crate::runtime::reservations::retire(host, &req.execution, req.retirement_mutation_id, req.expected_revision)))
        },
        "pane-runtime-execution-v4", "new-program-session" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::NewProgramSessionRequest, ctx| {
            let context = ctx.clone();
            Ok::<_, ServiceResponse>(crate::runtime::execution::create(host, req.name, req.options, move |id| {
                spawn::authorize_creation(&context, id, "session.create").map_err(crate::runtime::spawn_failure)
            }))
        },
        "pane-runtime-human-input-v1", "capabilities" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(human_input::capabilities())
        },
        "pane-runtime-human-input-v1", "open" => |req: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::client::OpenRequest, ctx| {
            Ok::<_, ServiceResponse>(human_input::open(host, ctx, req.execution))
        },
        "pane-runtime-human-input-v1", "push" => |req: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::client::PushRequest, ctx| {
            Ok::<_, ServiceResponse>(human_input::push(host, ctx, &req.stream, req.sequence, req.data))
        },
        "pane-runtime-human-input-v1", "close" => |req: bmux_pane_runtime_plugin_api::pane_runtime_human_input_v1::client::CloseRequest, ctx| {
            Ok::<_, ServiceResponse>(human_input::close(host, ctx, &req.stream))
        },
        "pane-runtime-execution-v4", "status" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::StatusRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::status(host, &req.execution)))
        },
        "pane-runtime-execution-v4", "screen" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ScreenRequest, ctx| {
            pointer_grid::legacy(execution::authorize(ctx, &req.execution, "read").and_then(|()| crate::runtime::execution::screen(host, &req.execution)))
        },
        "pane-runtime-execution-v4", "send" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::SendRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "pane.direct_input").and_then(|()| crate::runtime::execution::send(host, &req.execution, req.data, execution::input_authority(ctx, &req.execution)?)))
        },
        "pane-runtime-execution-v4", "resize" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::ResizeRequest, ctx| {
            Ok::<_, ServiceResponse>(execution::authorize(ctx, &req.execution, "pane.resize").and_then(|()| crate::runtime::execution::resize(host, &req.execution, req.cols, req.rows)))
        },
        "pane-runtime-execution-v4", "terminate" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::TerminateRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::terminate(host, &req.execution, req.grace_ms, || execution::authorize(ctx, &req.execution, "pane.close")))
        },
        "pane-runtime-execution-v4", "wait" => |req: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::client::WaitRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::execution::wait(host, &req.execution, req.timeout_ms, || execution::authorize(ctx, &req.execution, "read")))
        },
        "pane-runtime-spawn-commands-v1", "new-session" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::NewSessionRequest, ctx| {
            let result = crate::runtime::new_session_v1(host, req, |session_id| {
                spawn::authorize_creation(ctx, session_id, "session.create").map_err(crate::runtime::spawn_failure)
            }).map(|session_id| bmux_pane_runtime_plugin_api::pane_runtime_commands::SessionAck { session_id })
              .map_err(crate::runtime::spawn_wire_error);
            Ok::<_, ServiceResponse>(result)
        },
        "pane-runtime-spawn-commands-v1", "split-shell" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_commands_v1::client::SplitShellRequest, ctx| {
            let session_id = req.session_id;
            let result = crate::runtime::split_shell_v1(host, req, || {
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
            Ok::<_, ServiceResponse>(spawn::authorize_read(ctx, req.session_id).and_then(|()| crate::runtime::capture_spawn_source(host, req.session_id, req.pane_id)))
        },
        "pane-runtime-spawn-v1", "observe-directory" => |req: bmux_pane_runtime_plugin_api::pane_runtime_spawn_v1::client::ObserveDirectoryRequest, ctx| {
            Ok::<_, ServiceResponse>(spawn::authorize_read(ctx, req.source.session_id).and_then(|()| crate::runtime::observe_spawn_directory(host, &req.source)))
        },
        "pane-runtime-readiness-v1", "acquire-managed-preparation" => |req: bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::AcquireManagedPreparationRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::managed_idle::preparation(host, req.instance, ctx.caller_client_id, None, false).and_then(|lease| lease.ok_or_else(|| "preparation lease missing".into())))
        },
        "pane-runtime-readiness-v1", "renew-managed-preparation" => |req: bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::RenewManagedPreparationRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::managed_idle::preparation(host, req.instance, ctx.caller_client_id, Some(req.lease_id), false).and_then(|lease| lease.ok_or_else(|| "preparation lease missing".into())))
        },
        "pane-runtime-readiness-v1", "release-managed-preparation" => |req: bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::ReleaseManagedPreparationRequest, ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::managed_idle::preparation(host, req.instance, ctx.caller_client_id, Some(req.lease_id), true).map(|_| ()))
        },
        "pane-runtime-readiness-v1", "configure-managed-idle" => |req: bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::ConfigureManagedIdleRequest, _ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::managed_idle::configure(host, req.instance, req.grace_seconds))
        },
        "pane-runtime-readiness-v1", "managed-idle-status" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::managed_idle::inspect(host, None, false))
        },
        "pane-runtime-readiness-v1", "fence-managed-idle" => |req: bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::FenceManagedIdleRequest, _ctx| {
            Ok::<_, ServiceResponse>(crate::runtime::managed_idle::inspect(host, Some(req.instance), true))
        },
        "pane-runtime-readiness-v1", "runtime-readiness" => |_req: (), _ctx| {
            crate::readiness::snapshot(host).map_err(|reason| ServiceResponse::error("readiness_unavailable", reason))
        },
        "pane-runtime-readiness-v1", "fence-unhealthy" => |req: bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::FenceUnhealthyRequest, _ctx| {
            Ok::<_, ServiceResponse>(crate::readiness::fence(host, req.instance))
        },
        // pane-runtime-state queries.
        "pane-runtime-state", "pane-padding" => |req: padding::PanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::get(host, &req))
        },
        "pane-runtime-state", "padding-target-panes" => |req: padding::PaddingTargetPanesArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::target_panes(host, &req))
        },
        "pane-runtime-state", "list-panes" => |req: pane_state::ListPanesArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_panes(host, &req, ctx))
        },
        "pane-runtime-state", "get-pane" => |req: pane_state::GetPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_state::get_pane(host, &req, ctx))
        },
        "pane-runtime-state", "list-floating-panes" => |req: pane_state::ListFloatingPanesArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_floating_panes(host, &req))
        },
        "pane-runtime-state", "list-pane-processes" => |_req: (), _ctx| {
            Ok::<_, ServiceResponse>(pane_state::list_pane_processes(host))
        },
        "pane-runtime-state", "get-pane-process" => |req: pane_state::GetPaneProcessArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_state::get_pane_process(host, &req))
        },

        // pane-runtime-commands mutations.
        "pane-runtime-commands", "set-pane-padding" => |req: padding::SetPanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::set(host, &req))
        },
        "pane-runtime-commands", "clear-pane-padding" => |req: padding::PanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::clear(host, &req))
        },
        "pane-runtime-commands", "begin-pane-padding-preview" => |req: padding::BeginPanePaddingPreviewArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::begin_preview(host, &req, ctx))
        },
        "pane-runtime-commands", "update-pane-padding-preview" => |req: padding::UpdatePanePaddingPreviewArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::update_preview(host, &req, ctx))
        },
        "pane-runtime-commands", "cancel-pane-padding-preview" => |req: padding::PreviewTokenArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::cancel_preview(host, &req, ctx))
        },
        "pane-runtime-commands", "commit-pane-padding-preview" => |req: padding::CommitPanePaddingPreviewArgs, ctx| {
            Ok::<_, ServiceResponse>(padding::commit_preview(host, &req, ctx))
        },
        "pane-runtime-commands", "clear-pane-padding-overrides" => |req: padding::ClearPanePaddingOverridesArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::clear_overrides(host, &req))
        },
        "pane-runtime-commands", "set-global-pane-padding" => |req: padding::SetGlobalPanePaddingArgs, _ctx| {
            Ok::<_, ServiceResponse>(padding::set_global(host, &req))
        },
        "pane-runtime-commands", "split-pane" => |req: pane_commands::SplitPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::split_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "launch-pane" => |req: pane_commands::LaunchPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::launch_pane(host, req, ctx))
        },
        "pane-runtime-commands", "focus-pane" => |req: pane_commands::FocusPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::focus_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "resize-pane" => |req: pane_commands::ResizePaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::resize_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "close-pane" => |req: pane_commands::ClosePaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::close_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "restart-pane" => |req: pane_commands::RestartPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::restart_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "zoom-pane" => |req: pane_commands::ZoomPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::zoom_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "create-floating-pane" => |req: pane_commands::CreateFloatingPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::create_floating_pane(host, req, ctx))
        },
        "pane-runtime-commands", "move-floating-pane" => |req: pane_commands::MoveFloatingPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::move_floating_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "resize-floating-pane" => |req: pane_commands::ResizeFloatingPaneArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::resize_floating_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "focus-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::focus_floating_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "raise-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::raise_floating_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "lower-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::lower_floating_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "set-floating-pane-z" => |req: pane_commands::SetFloatingPaneZArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::set_floating_pane_z(host, &req, ctx))
        },
        "pane-runtime-commands", "set-floating-pane-layer" => |req: pane_commands::SetFloatingPaneLayerArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::set_floating_pane_layer(host, &req, ctx))
        },
        "pane-runtime-commands", "close-floating-pane" => |req: pane_commands::FloatingPaneTargetArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::close_floating_pane(host, &req, ctx))
        },
        "pane-runtime-commands", "pane-direct-input" => |req: pane_commands::PaneDirectInputArgs, ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_direct_input(host, req, ctx))
        },
        "pane-runtime-commands", "execution-pane-input" => |req: pane_commands::ExecutionPaneInputArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::execution_pane_input(host, req))
        },
        "pane-runtime-commands", "destroy-execution-pane" => |req: pane_commands::ExecutionPaneTargetArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::destroy_execution_pane(host, &req))
        },
        "pane-runtime-commands", "ensure-execution-pane" => |req: pane_commands::EnsureExecutionPaneArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::ensure_execution_pane(host, req))
        },
        "pane-runtime-commands", "pane-set-pty-size" => |req: pane_commands::PaneSetPtySizeArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_set_pty_size(host, &req))
        },
        "pane-runtime-commands", "pane-signal" => |req: pane_commands::PaneSignalArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::pane_signal(host, &req))
        },
        "pane-runtime-commands", "set-client-write-permission" => |req: pane_commands::SetClientWritePermissionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::set_client_write_permission(host, &req))
        },
        "pane-runtime-commands", "new-session-with-runtime" => |req: pane_commands::NewSessionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::new_session_with_runtime(host, &req))
        },
        "pane-runtime-commands", "kill-session-runtime" => |req: pane_commands::KillSessionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::kill_session_runtime(host, &req))
        },
        "pane-runtime-commands", "restore-session-runtime" => |_req: pane_commands::RestoreSessionArgs, _ctx| {
            Ok::<_, ServiceResponse>(pane_commands::restore_session_runtime())
        },

        // attach-runtime-commands.
        "attach-runtime-commands", "attach-session" => |req: attach_commands::AttachSessionArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_session(host, &req, ctx))
        },
        "attach-runtime-commands", "attach-context" => |req: attach_commands::AttachContextArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_context(host, &req, ctx))
        },
        "attach-runtime-commands", "observer-grant" => |req: attach_commands::ObserverGrantArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::observer_grant(host, &req, ctx))
        },
        "attach-runtime-commands", "observer-open" => |req: attach_commands::AttachOpenArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::observer_open(host, &req, ctx))
        },
        "attach-runtime-commands", "attach-open" => |req: attach_commands::AttachOpenArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_open(host, &req, ctx))
        },
        "attach-runtime-commands", "input-producer-barriers-v2" => |_req: serde_json::Value, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::input_producer_barriers(host, ctx))
        },
        "attach-runtime-commands", "acknowledge-input-producer-barrier-v2" => |req: attach_commands::AcknowledgeInputProducerBarrierArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::acknowledge_input_producer_barrier(host, &req, ctx))
        },
        "attach-runtime-commands", "suppress-pane-input-capture-v1" => |req: attach_commands::SuppressPaneInputCaptureArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::suppress_pane_input_capture(host, &req, ctx))
        },
        "attach-runtime-commands", "attach-input" => |req: attach_commands::AttachInputArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_input(host, req, ctx))
        },
        "attach-runtime-commands", "attach-output" => |req: attach_commands::AttachOutputArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_output(host, &req, ctx))
        },
        "attach-runtime-commands", "observe-viewport" => |req: attach_commands::ObserveViewportArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::observe_viewport(host, &req, ctx))
        },
        "attach-runtime-commands", "attach-set-viewport" => |req: attach_commands::AttachSetViewportArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_set_viewport(host, &req, ctx))
        },
        "attach-runtime-commands", "attach-retarget-context" => |req: attach_commands::AttachRetargetContextArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::attach_retarget_context(host, &req, ctx))
        },
        "attach-runtime-commands", "set-client-attach-policy" => |req: attach_commands::SetClientAttachPolicyArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::set_client_attach_policy(host, req, ctx))
        },
        "attach-runtime-commands", "detach" => |_req: attach_commands::DetachArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_commands::detach(host, ctx))
        },

        // attach-runtime-state queries.
        "attach-runtime-state", "attach-layout-state" => |req: attach_state::AttachLayoutArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_layout_state(host, &req, ctx))
        },
        "attach-runtime-state", "attach-snapshot-state" => |req: attach_state::AttachSnapshotArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_snapshot_state(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-snapshot-state" => |req: attach_state::AttachPaneSnapshotArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_snapshot_state(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-output-batch" => |req: attach_state::AttachPaneOutputBatchArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_output_batch(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-output-push" => |req: attach_state::AttachPaneOutputPushArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_output_push(host, &req, ctx))
        },
        "attach-runtime-state", "pane-output-cursor-state" => |req: attach_state::PaneOutputCursorArgs, _ctx| {
            Ok::<_, ServiceResponse>(attach_state::pane_output_cursor_state(host, &req))
        },
        "attach-runtime-state", "pane-grid-snapshot-state" => |req: attach_state::PaneGridSnapshotArgs, _ctx| {
            pointer_grid::legacy(attach_state::pane_grid_snapshot_state(host, &req))
        },
        "attach-runtime-state", "attach-pane-grid-snapshot-state" => |req: attach_state::AttachPaneGridSnapshotArgs, ctx| {
            pointer_grid::legacy(attach_state::attach_pane_grid_snapshot_state(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-grid-window-state" => |req: attach_state::AttachPaneGridWindowArgs, ctx| {
            pointer_grid::legacy(attach_state::attach_pane_grid_window_state(host, &req, ctx))
        },
        "attach-runtime-state", "attach-history-capture-v1" => |req: attach_state::AttachPaneScrollbackPinArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_capture(host, &req, ctx))
        },
        "attach-runtime-state", "attach-main-row-slice-v1" => |req: attach_state::HistorySliceArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::main_row_slice(host, &req, ctx))
        },
        "attach-runtime-state", "attach-history-slice-v1" => |req: attach_state::HistorySliceArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_slice(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-scrollback-pin" => |req: attach_state::AttachPaneScrollbackPinArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_scrollback_pin(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-scrollback-unpin" => |req: attach_state::AttachPaneScrollbackUnpinArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_scrollback_unpin(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-frames" => |req: attach_state::AttachPaneFramesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_frames(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-frames-reset" => |req: attach_state::AttachPaneFramesResetArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_frames_reset(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-frame-push" => |req: attach_state::AttachPaneFramesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_frame_push(host, &req, ctx))
        },
        "attach-runtime-state", "observe-pane-frames" => |req: attach_state::ObservePaneFramesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::observe_pane_frames(host, &req, ctx))
        },
        "attach-runtime-state", "observe-pane-frames-reset" => |req: attach_state::ObservePaneFramesResetArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::observe_pane_frames_reset(host, &req, ctx))
        },
        "attach-runtime-state", "attach-history-images-v1" => |req: attach_state::HistoryImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_images(host, &req, ctx))
        },
        "attach-runtime-state", "attach-history-images-v2" => |req: attach_state::HistoryImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_images_reflowed(host, &req, ctx))
        },
        "attach-runtime-state", "attach-history-images-v3" => |req: attach_state::AnchoredHistoryImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::history_images_anchored(host, &req, ctx))
        },
        "attach-runtime-state", "attach-pane-images" => |req: attach_state::AttachPaneImagesArgs, ctx| {
            Ok::<_, ServiceResponse>(attach_state::attach_pane_images(host, &req, ctx))
        },
    })
}

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
pub(crate) fn publish_focus_state_snapshot(host: &crate::runtime::PaneRuntimeHost) {
    let Some(handle) = host.registered_session_runtime() else {
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
    // Per-activation monotonic counter so subscribers can order updates.
    let revision = host.next_focus_revision();
    let payload = SessionFocusStateMap { entries, revision };
    // Ignore publish errors — the channel is registered in `activate`
    // before any mutation can run; a failure here means the plugin
    // was dropped, and there's nothing to do.
    if let Some(events) = host.events().cloned() {
        let _ = events.publish_state(&FOCUS_STATE_KIND, payload);
    }
}
