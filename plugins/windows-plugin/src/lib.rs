#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

mod domain_ipc;

use bmux_plugin::{HostRuntimeApi, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{
    HostScope, StorageGetRequest, StorageSetRequest, TypedServiceRegistrationContext,
    TypedServiceRegistry,
};
use bmux_windows_plugin_api::windows_commands::{
    self, CloseError, FocusError, PaneAck, PaneDirection, PaneMutationError, PaneZoomAck, Selector,
    WindowAck, WindowError, WindowsCommandsService,
};
use bmux_windows_plugin_api::windows_state::{self, PaneState, WindowEntry, WindowsStateService};
use domain_ipc::KernelOps;
use domain_ipc::{ContextCloseRequest, ContextCreateRequest, ContextSelector};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

const ACTIVE_WINDOW_CONTEXT_KEY: &str = "windows.active_context_id";
const PREVIOUS_WINDOW_CONTEXT_KEY: &str = "windows.previous_context_id";
const WINDOW_ORDER_KEY: &str = "windows.order";
const ATTACH_PHASE_MARKER: &str = "[bmux-attach-phase-json]";

fn emit_attach_phase_timing(payload: &serde_json::Value) {
    if std::env::var_os("BMUX_ATTACH_PHASE_TIMING").is_none() {
        return;
    }
    eprintln!("{ATTACH_PHASE_MARKER}{payload}");
}

/// Shared "last selected pane per client" map. Mutated by the
/// byte-encoded `switch-window` handler (via the plugin's mutable
/// access in `invoke_service`) AND by the typed
/// [`WindowsCommandsService::switch_window`] impl (via a clone of the
/// same [`Arc<Mutex<_>>`]). Both paths observe the same state.
type LastSelectedByClient = Arc<Mutex<BTreeMap<Uuid, Uuid>>>;

#[derive(Default)]
pub struct WindowsPlugin {
    last_selected_by_client: LastSelectedByClient,
}

impl RustPlugin for WindowsPlugin {
    fn activate(&mut self, _context: NativeLifecycleContext) -> Result<i32, PluginCommandError> {
        // Register the typed event-bus channel for pane-event so
        // subscribers (decoration, future UI plugins) can wait on
        // `global_event_bus().subscribe::<PaneEvent>(...)` without
        // racing the first emit. Failure to register is non-fatal —
        // the channel may already exist from a prior load.
        let _ = bmux_plugin::global_event_bus()
            .register_channel::<bmux_windows_plugin_api::windows_events::PaneEvent>(
                bmux_windows_plugin_api::windows_events::EVENT_KIND,
            );

        // Register the reactive state channel carrying the ordered
        // window list. The attach tab bar subscribes via
        // `subscribe_state::<WindowListSnapshot>` and observes every
        // order mutation without polling. Seed with an empty snapshot
        // — the first real publish happens on the first mutation
        // (new-window / switch-window / …). If a consumer activates
        // before the first mutation they see an empty list, which
        // correctly reflects that no windows exist yet.
        bmux_plugin::global_event_bus()
            .register_state_channel::<bmux_windows_plugin_api::windows_list::WindowListSnapshot>(
                bmux_windows_plugin_api::windows_list::STATE_KIND,
                bmux_windows_plugin_api::windows_list::WindowListSnapshot {
                    windows: Vec::new(),
                    revision: 0,
                },
            );
        Ok(EXIT_OK)
    }

    fn run_command(&mut self, context: NativeCommandContext) -> Result<i32, PluginCommandError> {
        handle_command(self, &context)?;
        Ok(EXIT_OK)
    }

    #[allow(clippy::too_many_lines)] // route_service! covers every windows-commands op; the block is naturally long.
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "windows-state", "list-windows" => |req: ListWindowsArgs, ctx| {
                let windows = list_windows(ctx, req.session.as_deref())
                    .map_err(|e| ServiceResponse::error("list_failed", e))?;
                Ok(windows)
            },
            "windows-commands", "new-window" => |req: NewWindowArgs, ctx| {
                create_window(ctx, req.name)
                    .map_err(|e| ServiceResponse::error("new_failed", e))
            },
            "windows-commands", "kill-window" => |req: KillWindowArgs, ctx| {
                let selector = parse_selector(&req.target)
                    .map_err(|e| ServiceResponse::error("invalid_request", e))?;
                kill_window(ctx, selector, req.force_local)
                    .map_err(|e| ServiceResponse::error("kill_failed", e))
            },
            "windows-commands", "kill-all-windows" => |req: KillAllWindowsArgs, ctx| {
                kill_all_windows(ctx, req.force_local)
                    .map_err(|e| ServiceResponse::error("kill_failed", e))
            },
            "windows-commands", "switch-window" => |req: SwitchWindowArgs, ctx| {
                let selector = parse_selector(&req.target)
                    .map_err(|e| ServiceResponse::error("invalid_request", e))?;
                switch_window(ctx, selector, &self.last_selected_by_client)
                    .map_err(|e| ServiceResponse::error("switch_failed", e))
            },
            "windows-commands", "focus-pane" => |req: FocusPaneArgs, ctx| {
                let request = domain_ipc::PaneFocusRequest {
                    session: None,
                    target: Some(domain_ipc::PaneSelector::ById(req.id)),
                    direction: None,
                };
                ctx.pane_focus(&request)
                    .map(|_| PaneAck { ok: true, pane_id: Some(req.id) })
                    .map_err(|e| ServiceResponse::error("focus_failed", e.to_string()))
            },
            "windows-commands", "close-pane" => |req: ClosePaneArgs, ctx| {
                let request = domain_ipc::PaneCloseRequest {
                    session: None,
                    target: Some(domain_ipc::PaneSelector::ById(req.id)),
                };
                ctx.pane_close(&request)
                    .map(|_| PaneAck { ok: true, pane_id: Some(req.id) })
                    .map_err(|e| ServiceResponse::error("close_failed", e.to_string()))
            },
            "windows-commands", "focus-pane-by-selector" => |req: FocusPaneBySelectorArgs, ctx| {
                let request = domain_ipc::PaneFocusRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: Some(selector_to_pane(&req.target)),
                    direction: None,
                };
                ctx.pane_focus(&request)
                    .map(|resp| PaneAck { ok: true, pane_id: Some(resp.id) })
                    .map_err(|e| ServiceResponse::error("focus_failed", e.to_string()))
            },
            "windows-commands", "close-pane-by-selector" => |req: ClosePaneBySelectorArgs, ctx| {
                let request = domain_ipc::PaneCloseRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: Some(selector_to_pane(&req.target)),
                };
                ctx.pane_close(&request)
                    .map(|resp| PaneAck { ok: true, pane_id: Some(resp.id) })
                    .map_err(|e| ServiceResponse::error("close_failed", e.to_string()))
            },
            "windows-commands", "close-active-pane" => |req: CloseActivePaneArgs, ctx| {
                let request = domain_ipc::PaneCloseRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: None,
                };
                ctx.pane_close(&request)
                    .map(|resp| PaneAck { ok: true, pane_id: Some(resp.id) })
                    .map_err(|e| ServiceResponse::error("close_failed", e.to_string()))
            },
            "windows-commands", "focus-pane-in-direction" => |req: FocusPaneInDirectionArgs, ctx| {
                let Some(focus_dir) = pane_direction_to_focus(req.direction) else {
                    return Err(ServiceResponse::error(
                        "invalid_request",
                        "direction must be Next/Prev (Horizontal/Vertical aren't meaningful)",
                    ));
                };
                let request = domain_ipc::PaneFocusRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: None,
                    direction: Some(focus_dir),
                };
                ctx.pane_focus(&request)
                    .map(|resp| PaneAck { ok: true, pane_id: Some(resp.id) })
                    .map_err(|e| ServiceResponse::error("focus_failed", e.to_string()))
            },
            "windows-commands", "split-pane" => |req: SplitPaneArgs, ctx| {
                let request = domain_ipc::PaneSplitRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: req.target.as_ref().map(selector_to_pane),
                    direction: pane_direction_to_split(req.direction),
                };
                ctx.pane_split(&request)
                    .map(|resp| PaneAck { ok: true, pane_id: Some(resp.id) })
                    .map_err(|e| ServiceResponse::error("split_failed", e.to_string()))
            },
            "windows-commands", "launch-pane" => |req: LaunchPaneArgs, ctx| {
                let request = domain_ipc::PaneLaunchRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: req.target.as_ref().map(selector_to_pane),
                    direction: pane_direction_to_split(req.direction),
                    name: req.name,
                    command: domain_ipc::PaneLaunchCommand {
                        program: req.program,
                        args: req.args,
                        cwd: None,
                        env: BTreeMap::new(),
                    },
                };
                ctx.pane_launch(&request)
                    .map(|resp| PaneAck { ok: true, pane_id: Some(resp.id) })
                    .map_err(|e| ServiceResponse::error("launch_failed", e.to_string()))
            },
            "windows-commands", "resize-pane" => |req: ResizePaneArgs, ctx| {
                let request = domain_ipc::PaneResizeRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                    target: req.target.as_ref().map(selector_to_pane),
                    delta: req.delta,
                };
                ctx.pane_resize(&request)
                    .map(|_| PaneAck { ok: true, pane_id: None })
                    .map_err(|e| ServiceResponse::error("resize_failed", e.to_string()))
            },
            "windows-commands", "zoom-pane" => |req: ZoomPaneArgs, ctx| {
                let request = domain_ipc::PaneZoomRequest {
                    session: req.session.as_ref().and_then(selector_to_session),
                };
                ctx.pane_zoom(&request)
                    .map(|resp| PaneZoomAck {
                        pane_id: resp.pane_id,
                        zoomed: resp.zoomed,
                    })
                    .map_err(|e| ServiceResponse::error("zoom_failed", e.to_string()))
            },
            "windows-commands", "restart-pane" => |_req: RestartPaneArgs, _ctx| {
                Err::<PaneAck, _>(ServiceResponse::error(
                    "unsupported",
                    "restart-pane typed command is not wired to a host primitive yet",
                ))
            },
        })
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        // Provider handles share the same `LastSelectedByClient` map
        // as the byte-encoded path on `WindowsPlugin` so state stays
        // consistent between transports.
        let shared = WindowsSharedState {
            caller: Arc::new(TypedServiceCaller::from_registration_context(&context)),
            last_selected_by_client: self.last_selected_by_client.clone(),
        };

        let (Ok(read_cap), Ok(write_cap)) = (
            HostScope::new(bmux_windows_plugin_api::capabilities::WINDOWS_READ.as_str()),
            HostScope::new(bmux_windows_plugin_api::capabilities::WINDOWS_WRITE.as_str()),
        ) else {
            return;
        };

        let commands: Arc<dyn WindowsCommandsService + Send + Sync> =
            Arc::new(WindowsCommandsHandle::new(shared.clone()));
        registry.insert_typed::<dyn WindowsCommandsService + Send + Sync>(
            write_cap,
            ServiceKind::Command,
            windows_commands::INTERFACE_ID,
            commands,
        );

        let state: Arc<dyn WindowsStateService + Send + Sync> =
            Arc::new(WindowsStateHandle::new(shared.clone()));
        registry.insert_typed::<dyn WindowsStateService + Send + Sync>(
            read_cap,
            ServiceKind::Query,
            windows_state::INTERFACE_ID,
            state,
        );

        // Spawn the contexts-events subscriber. The windows plugin is
        // an authoritative projection of context lifecycle: every
        // Created/Closed/Selected/SessionActiveContextChanged event
        // flows through here and updates `windows.order` + the
        // `windows-list` state channel.
        //
        // Subscription happens here (not in `activate`) because
        // `TypedServiceCaller::from_registration_context` needs the
        // typed registration context that `activate` does not receive.
        spawn_contexts_events_subscriber(shared.clone());

        // Publish the initial window-list snapshot populated from the
        // plugin's persisted `windows.order` storage projected through
        // the current context list. The `register_state_channel` call
        // in `activate` registered an empty placeholder because
        // `activate` has no host access; now that we have a
        // `TypedServiceCaller` we publish the authoritative state
        // synchronously so:
        //
        //   - The server's `spawn_plugin_bus_state_forwarder` (which
        //     runs after us in bootstrap) reads the populated value
        //     when it calls `subscribe_state` to capture `initial`.
        //   - Attach clients connecting afterward see the correct
        //     tab order on first frame — no flash of `1:terminal`
        //     even when the server starts with pre-existing contexts
        //     restored from a prior session.
        //
        // `windows.order` is persisted under
        // `<data_dir>/plugin-storage/bmux.windows/windows.order.bin`
        // by the kernel storage service, so the user sees their tab
        // order exactly as they left it before the server shutdown.
        publish_window_list_snapshot(shared.caller.as_ref());
    }
}

/// Spawn a dedicated thread that subscribes to `contexts-events` and
/// drives windows-plugin state transitions.
///
/// The thread owns a current-thread tokio runtime so it can `await`
/// on the subscription's `recv` without interfering with host
/// scheduling. It runs until the plugin process terminates.
fn spawn_contexts_events_subscriber(shared: WindowsSharedState) {
    use bmux_contexts_plugin_api::contexts_events::{self, ContextEvent};

    let subscribe_result =
        bmux_plugin::global_event_bus().subscribe::<ContextEvent>(&contexts_events::EVENT_KIND);
    let mut rx = if let Ok(rx) = subscribe_result {
        rx
    } else {
        // contexts-plugin hasn't registered its channel yet
        // (unusual — should be a load-order issue). We retry once
        // on a short delay; if that fails too, give up. Without
        // the subscription, windows.order only updates when the
        // user invokes windows-plugin commands directly.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let Ok(rx) =
            bmux_plugin::global_event_bus().subscribe::<ContextEvent>(&contexts_events::EVENT_KIND)
        else {
            return;
        };
        rx
    };

    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            while let Ok(event) = rx.recv().await {
                handle_context_event(&shared, &event);
            }
        });
    });
}

/// Dispatch a single `ContextEvent` against the windows-plugin's
/// persisted window order + active marker. Every handler ends with a
/// `publish_window_list_snapshot` so subscribers of the `windows-list`
/// state channel see the update without polling.
fn handle_context_event(
    shared: &WindowsSharedState,
    event: &bmux_contexts_plugin_api::contexts_events::ContextEvent,
) {
    use bmux_contexts_plugin_api::contexts_events::ContextEvent;

    let caller = shared.caller.as_ref();
    match event {
        ContextEvent::Created { context_id, .. } => {
            let _ = append_context_to_window_order(caller, *context_id);
            publish_window_list_snapshot(caller);
        }
        ContextEvent::Closed { context_id } => {
            let _ = remove_context_from_window_order(caller, *context_id);
            publish_window_list_snapshot(caller);
        }
        ContextEvent::Selected { context_id }
        | ContextEvent::SessionActiveContextChanged { context_id, .. } => {
            let _ = mark_context_active(caller, *context_id);
            publish_window_list_snapshot(caller);
        }
    }
}

/// Append `context_id` to the persisted `windows.order` list when it
/// is not already present. Preserves the existing order of every
/// already-known entry — new contexts land at the end, matching the
/// creation order of the `ContextEvent::Created` stream.
fn append_context_to_window_order(
    caller: &impl HostRuntimeApi,
    context_id: Uuid,
) -> Result<(), String> {
    let mut order_ids = get_stored_window_order_ids(caller)?;
    if order_ids.contains(&context_id) {
        return Ok(());
    }
    order_ids.push(context_id);
    set_stored_window_order_ids(caller, &order_ids)
}

/// Remove `context_id` from the persisted `windows.order` list.
/// No-op when the id is not present. Also clears the active marker
/// if it was pointing at the removed context.
fn remove_context_from_window_order(
    caller: &impl HostRuntimeApi,
    context_id: Uuid,
) -> Result<(), String> {
    let mut order_ids = get_stored_window_order_ids(caller)?;
    let len_before = order_ids.len();
    order_ids.retain(|id| *id != context_id);
    if order_ids.len() == len_before {
        // Not in list — nothing to persist. Still clear active if it
        // matches, below.
    } else {
        set_stored_window_order_ids(caller, &order_ids)?;
    }
    // Clear active marker if it points at the removed context.
    if let Ok(Some(active)) = get_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY)
        && active == context_id
    {
        let _ = set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, None);
    }
    if let Ok(Some(previous)) = get_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY)
        && previous == context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, None);
    }
    Ok(())
}

/// Update `ACTIVE_WINDOW_CONTEXT_KEY` to `context_id`, moving the
/// previous active context (if any and different) into
/// `PREVIOUS_WINDOW_CONTEXT_KEY` so `last-window` still works.
fn mark_context_active(caller: &impl HostRuntimeApi, context_id: Uuid) -> Result<(), String> {
    let previous = get_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY)
        .ok()
        .flatten();
    if let Some(previous) = previous
        && previous != context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, Some(previous));
    }
    set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, Some(context_id))
}

#[allow(clippy::too_many_lines)]
fn handle_command(plugin: &WindowsPlugin, context: &NativeCommandContext) -> Result<(), String> {
    // Only emit confirmation text to stdout when invoked from a
    // standalone CLI (e.g. `bmux window new`). When this plugin is
    // dispatched from an attach keybinding the host is rendering a
    // raw-mode TUI and `println!` would paint over pane content; the
    // attach runtime observes state changes (current context id,
    // context list) directly and refreshes from those, so silence is
    // correct there.
    let emit_to_stdout = matches!(
        context.invocation_source,
        bmux_plugin_sdk::NativeCommandInvocationSource::Cli
    );
    match context.command.as_str() {
        "new-window" => {
            let name = option_value(&context.arguments, "name");
            let ack = create_window(context, name)?;
            if emit_to_stdout && let Some(context_id) = ack.id {
                println!("created window context: {context_id}");
            }
            Ok(())
        }
        "list-windows" => {
            let session_filter = option_value(&context.arguments, "session");
            let as_json = has_flag(&context.arguments, "json");
            let windows = list_windows(context, session_filter.as_deref())?;
            if !emit_to_stdout {
                // Rendering list output is only meaningful from the
                // CLI; attach keybindings don't have a useful surface
                // for it here and the attach UI refreshes its own
                // state from the contexts/sessions catalogs.
                return Ok(());
            }
            if as_json {
                let output =
                    serde_json::to_string_pretty(&serde_json::json!({ "windows": windows }))
                        .map_err(|error| error.to_string())?;
                println!("{output}");
            } else if windows.is_empty() {
                println!("no windows");
            } else {
                for window in windows {
                    println!(
                        "{}\t{}\t{}",
                        window.id,
                        window.name,
                        if window.active { "active" } else { "inactive" }
                    );
                }
            }
            Ok(())
        }
        "kill-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let force_local = has_flag(&context.arguments, "force-local");
            let response = context
                .context_close(&ContextCloseRequest {
                    selector,
                    force: force_local,
                })
                .map_err(|error| error.to_string())?;
            if emit_to_stdout {
                println!("killed window context: {}", response.id);
            }
            Ok(())
        }
        "kill-all-windows" => {
            let force_local = has_flag(&context.arguments, "force-local");
            let contexts = context
                .context_list()
                .map_err(|error| error.to_string())?
                .contexts;
            if contexts.is_empty() {
                if emit_to_stdout {
                    println!("no windows");
                }
                return Ok(());
            }
            for context_summary in contexts {
                let response = context
                    .context_close(&ContextCloseRequest {
                        selector: ContextSelector::ById(context_summary.id),
                        force: force_local,
                    })
                    .map_err(|error| error.to_string())?;
                if emit_to_stdout {
                    println!("killed window context: {}", response.id);
                }
            }
            Ok(())
        }
        "switch-window" => {
            let target = positional_value(&context.arguments)
                .ok_or_else(|| "missing required TARGET argument".to_string())?;
            let selector = parse_selector(&target)?;
            let ack = switch_window(context, selector, &plugin.last_selected_by_client)?;
            let context_id = ack
                .id
                .ok_or_else(|| "switch-window did not return selected context id".to_string())?;
            if emit_to_stdout {
                println!("active window context: {context_id}");
            }
            Ok(())
        }
        "next-window" => {
            let ack = cycle_window(
                context,
                WindowCycleDirection::Next,
                &plugin.last_selected_by_client,
            )?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("next-window selected context {id}");
            }
            Ok(())
        }
        "prev-window" => {
            let ack = cycle_window(
                context,
                WindowCycleDirection::Previous,
                &plugin.last_selected_by_client,
            )?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("prev-window selected context {id}");
            }
            Ok(())
        }
        "last-window" => {
            let ack = cycle_window(
                context,
                WindowCycleDirection::Last,
                &plugin.last_selected_by_client,
            )?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("last-window selected context {id}");
            }
            Ok(())
        }
        "goto-window" => {
            let index_str = positional_value(&context.arguments)
                .ok_or_else(|| "missing required INDEX argument".to_string())?;
            let index: usize = index_str.parse().map_err(|_| {
                format!("invalid window index '{index_str}' (expected 1-based number)")
            })?;
            if index == 0 {
                return Err("window index must be 1 or greater".to_string());
            }
            let ack = goto_window_by_index(context, index, &plugin.last_selected_by_client)?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("goto-window {index} selected context {id}");
            }
            Ok(())
        }
        "close-current-window" => {
            let ack = close_current_window(context, &plugin.last_selected_by_client)?;
            if emit_to_stdout && let Some(id) = ack.id {
                println!("closed current window context {id}");
            }
            Ok(())
        }
        "reset-order" => {
            let count = reset_window_order(context)?;
            if emit_to_stdout {
                println!("reset window order; rebuilt {count} windows");
            }
            Ok(())
        }
        // ── Pane-level commands (promoted from service handlers) ──
        //
        // Each of these dispatches to the same typed-service logic
        // implemented in `invoke_service`, but via a command-style
        // entry so keybindings can reach them through
        // `plugin:bmux.windows:<name>`. The handlers forward to the
        // `HostRuntimeApi::pane_*` trait methods which ultimately
        // route through the windows-plugin service boundary.
        //
        // Keybindings do not pass a `--session` arg (the attach
        // runtime always operates on the currently-attached session),
        // so we pass `session: None` to the underlying request and
        // rely on the host to resolve to the caller's attached
        // session.
        "focus-pane-in-direction" => {
            let direction = option_value(&context.arguments, "direction")
                .ok_or_else(|| "--direction is required".to_string())?;
            let direction = parse_pane_direction_arg(&direction)?;
            let focus_dir = pane_direction_to_focus(direction).ok_or_else(|| {
                "direction must be left/right/up/down/next/prev (horizontal/vertical are split-only)".to_string()
            })?;
            let request = domain_ipc::PaneFocusRequest {
                session: None,
                target: None,
                direction: Some(focus_dir),
            };
            context.pane_focus(&request).map_err(|e| e.to_string())?;
            Ok(())
        }
        "split-pane" => {
            let direction = option_value(&context.arguments, "direction")
                .ok_or_else(|| "--direction is required".to_string())?;
            let direction = parse_pane_direction_arg(&direction)?;
            let request = domain_ipc::PaneSplitRequest {
                session: None,
                target: None,
                direction: pane_direction_to_split(direction),
            };
            context.pane_split(&request).map_err(|e| e.to_string())?;
            Ok(())
        }
        "resize-pane" => {
            let direction_arg = option_value(&context.arguments, "direction");
            // Translate direction to a delta:
            //   - increase / right / down → +1
            //   - decrease / left / up   → -1
            // The pane-runtime picks the axis from the focused split;
            // the delta sign controls grow-vs-shrink.
            //
            // Arms are grouped by user intent (one per direction
            // keyword), not by return value, so we keep them split
            // rather than collapsing by shared body.
            #[allow(clippy::match_same_arms)]
            let delta: i16 = match direction_arg.as_deref() {
                Some("increase" | "right" | "down") => 1,
                Some("decrease" | "left" | "up") => -1,
                Some(other) => {
                    return Err(format!(
                        "unknown resize direction '{other}' (expected increase/decrease/left/right/up/down)"
                    ));
                }
                None => 1,
            };
            let request = domain_ipc::PaneResizeRequest {
                session: None,
                target: None,
                delta,
            };
            context.pane_resize(&request).map_err(|e| e.to_string())?;
            Ok(())
        }
        "zoom-pane" => {
            let request = domain_ipc::PaneZoomRequest { session: None };
            context.pane_zoom(&request).map_err(|e| e.to_string())?;
            Ok(())
        }
        "close-active-pane" => {
            let request = domain_ipc::PaneCloseRequest {
                session: None,
                target: None,
            };
            context.pane_close(&request).map_err(|e| e.to_string())?;
            Ok(())
        }
        "restart-pane" => {
            // Restart wiring is a pre-existing stub; we return a clean
            // error until the host primitive lands.
            Err("restart-pane is not yet wired to a host primitive".to_string())
        }
        _ => Err(format!("unsupported command '{}'", context.command)),
    }
}

#[derive(Debug)]
enum WindowCycleDirection {
    Next,
    Previous,
    Last,
}

fn list_windows(
    caller: &impl HostRuntimeApi,
    session_filter: Option<&str>,
) -> Result<Vec<WindowEntry>, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let contexts = order_contexts_for_navigation(caller, contexts)?;
    let selected = if let Some(filter) = session_filter {
        let selector = parse_selector(filter)?;
        contexts
            .into_iter()
            .filter(|context| match &selector {
                ContextSelector::ById(id) => &context.id == id,
                ContextSelector::ByName(name) => context.name.as_deref() == Some(name.as_str()),
            })
            .collect::<Vec<_>>()
    } else {
        contexts
    };
    let current_context = resolve_effective_current_context_with_contexts(caller, &selected)?;

    Ok(selected
        .into_iter()
        .enumerate()
        .map(|(index, context)| WindowEntry {
            id: context.id.to_string(),
            name: context
                .name
                .unwrap_or_else(|| format!("tab-{}", index.saturating_add(1))),
            active: current_context == Some(context.id),
        })
        .collect())
}

/// Monotonic counter for the windows-list state channel.
///
/// Advanced once per [`publish_window_list_snapshot`] call so
/// subscribers can deduplicate or order updates without relying on
/// wall-clock time.
static WINDOW_LIST_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Publish the current ordered window list on the `windows-list`
/// state channel.
///
/// Called by every window-order-mutating code path (`create_window`,
/// `switch_window`, `kill_window`, `kill_all_windows`,
/// `goto_window_by_index`, `cycle_window`, `close_current_window`) so
/// subscribers (the attach tab bar, future UI plugins) observe the
/// current order synchronously on `subscribe_state` and receive live
/// updates on every mutation — no polling.
///
/// Silently no-ops when the underlying `list_windows` call fails or
/// when the state channel has not been registered (plugin not yet
/// activated). The channel is seeded empty in `activate`, so once the
/// plugin is active this publish always succeeds.
fn publish_window_list_snapshot(caller: &impl HostRuntimeApi) {
    let Ok(entries) = list_windows(caller, None) else {
        return;
    };
    let windows: Vec<bmux_windows_plugin_api::windows_list::WindowListEntry> = entries
        .into_iter()
        .filter_map(|entry| {
            let id = Uuid::parse_str(&entry.id).ok()?;
            Some(bmux_windows_plugin_api::windows_list::WindowListEntry {
                id,
                name: entry.name,
                active: entry.active,
            })
        })
        .collect();
    let revision = WINDOW_LIST_REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let snapshot = bmux_windows_plugin_api::windows_list::WindowListSnapshot { windows, revision };
    let _ = bmux_plugin::global_event_bus()
        .publish_state(&bmux_windows_plugin_api::windows_list::STATE_KIND, snapshot);
}

/// Clear persisted `windows.order` and rebuild deterministically from
/// the current context list.
///
/// Serves as an escape hatch for users whose windows.order got
/// scrambled by pre-event-driven code paths (legacy bug). Ordering is
/// reconstructed from the context list sorted by UUID, so every
/// invocation produces the same result given the same input — but it
/// is NOT guaranteed to match creation order. Users who want exact
/// creation order should recreate their contexts after reset.
///
/// Returns the count of contexts written to the new order.
fn reset_window_order(caller: &impl HostRuntimeApi) -> Result<usize, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let mut ids: Vec<Uuid> = contexts.iter().map(|context| context.id).collect();
    ids.sort_by_key(uuid::Uuid::as_u128);
    set_stored_window_order_ids(caller, &ids)?;
    publish_window_list_snapshot(caller);
    Ok(ids.len())
}

fn create_window(caller: &impl HostRuntimeApi, name: Option<String>) -> Result<WindowAck, String> {
    let resolved_name = name.or_else(|| {
        caller
            .context_list()
            .ok()
            .map(|response| next_default_tab_name_for_contexts(&response.contexts))
    });
    let previous_context = resolve_effective_current_context(caller).ok().flatten();
    let response = caller
        .context_create(&ContextCreateRequest {
            name: resolved_name,
            attributes: BTreeMap::new(),
        })
        .map_err(|error| error.to_string())?;
    let context_id = response.context.id;
    if let Some(previous) = previous_context {
        append_context_to_window_order(caller, previous)?;
    }
    append_context_to_window_order(caller, context_id)?;
    if let Some(previous) = previous_context
        && previous != context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, Some(previous));
    }
    let _ = set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, Some(context_id));
    publish_window_list_snapshot(caller);
    Ok(WindowAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn next_default_tab_name_for_contexts(contexts: &[domain_ipc::ContextSummary]) -> String {
    let mut next = 1_u32;
    loop {
        let candidate = format!("tab-{next}");
        if contexts
            .iter()
            .all(|context| context.name.as_deref() != Some(candidate.as_str()))
        {
            return candidate;
        }
        next = next.saturating_add(1);
    }
}

fn kill_window(
    caller: &impl HostRuntimeApi,
    selector: ContextSelector,
    force_local: bool,
) -> Result<WindowAck, String> {
    let response = caller
        .context_close(&ContextCloseRequest {
            selector,
            force: force_local,
        })
        .map_err(|error| error.to_string())?;
    publish_window_list_snapshot(caller);
    Ok(WindowAck {
        ok: true,
        id: Some(response.id.to_string()),
    })
}

fn kill_all_windows(caller: &impl HostRuntimeApi, force_local: bool) -> Result<WindowAck, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    for context in contexts {
        caller
            .context_close(&ContextCloseRequest {
                selector: ContextSelector::ById(context.id),
                force: force_local,
            })
            .map_err(|error| error.to_string())?;
    }
    publish_window_list_snapshot(caller);
    Ok(WindowAck { ok: true, id: None })
}

#[allow(clippy::needless_pass_by_value)] // Plugin command dispatch passes owned selector from deserialized request
fn switch_window(
    caller: &impl HostRuntimeApi,
    selector: ContextSelector,
    last_selected_by_client: &LastSelectedByClient,
) -> Result<WindowAck, String> {
    if let ContextSelector::ById(context_id) = selector {
        return switch_window_by_id_fast(caller, context_id, last_selected_by_client);
    }
    let total_started = Instant::now();
    let list_started = Instant::now();
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let context_list_us = list_started.elapsed().as_micros();
    switch_window_with_contexts(
        caller,
        &selector,
        last_selected_by_client,
        &contexts,
        context_list_us,
        total_started,
    )
}

fn switch_window_by_id_fast(
    caller: &impl HostRuntimeApi,
    context_id: Uuid,
    last_selected_by_client: &LastSelectedByClient,
) -> Result<WindowAck, String> {
    let total_started = Instant::now();
    let current_started = Instant::now();
    let previous_context = caller
        .context_current()
        .map_err(|error| error.to_string())?
        .context
        .map(|context| context.id);
    let current_context_us = current_started.elapsed().as_micros();
    let select_started = Instant::now();
    caller
        .context_select(&domain_ipc::ContextSelectRequest {
            selector: ContextSelector::ById(context_id),
        })
        .map_err(|error| error.to_string())?;
    let context_select_us = select_started.elapsed().as_micros();
    let remember_started = Instant::now();
    if let Ok(client) = caller.current_client()
        && let Some(previous) = previous_context
        && previous != context_id
        && let Ok(mut map) = last_selected_by_client.lock()
    {
        map.insert(client.id, previous);
    }
    if let Some(previous) = previous_context
        && previous != context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, Some(previous));
    }
    let _ = set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, Some(context_id));
    let remember_us = remember_started.elapsed().as_micros();
    let publish_started = Instant::now();
    publish_window_list_snapshot(caller);
    let publish_us = publish_started.elapsed().as_micros();
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "windows.switch_window",
        "fast_by_id": true,
        "previous_context_id": previous_context,
        "selected_context_id": context_id,
        "context_count": serde_json::Value::Null,
        "context_list_us": 0_u128,
        "current_context_us": current_context_us,
        "resolve_us": 0_u128,
        "context_select_us": context_select_us,
        "remember_us": remember_us,
        "publish_us": publish_us,
        "total_us": total_started.elapsed().as_micros(),
    }));
    Ok(WindowAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

fn switch_window_with_contexts(
    caller: &impl HostRuntimeApi,
    selector: &ContextSelector,
    last_selected_by_client: &LastSelectedByClient,
    contexts: &[domain_ipc::ContextSummary],
    context_list_us: u128,
    total_started: Instant,
) -> Result<WindowAck, String> {
    let resolve_started = Instant::now();
    let previous_context = resolve_effective_current_context_with_contexts(caller, contexts)?;
    let context_id = resolve_context_id_from_contexts(contexts, selector)?;
    let resolve_us = resolve_started.elapsed().as_micros();
    let select_started = Instant::now();
    caller
        .context_select(&domain_ipc::ContextSelectRequest {
            selector: ContextSelector::ById(context_id),
        })
        .map_err(|error| error.to_string())?;
    let context_select_us = select_started.elapsed().as_micros();
    let remember_started = Instant::now();
    if let Ok(client) = caller.current_client()
        && let Some(previous) = previous_context
        && previous != context_id
        && let Ok(mut map) = last_selected_by_client.lock()
    {
        map.insert(client.id, previous);
    }
    if let Some(previous) = previous_context
        && previous != context_id
    {
        let _ = set_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY, Some(previous));
    }
    let _ = set_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY, Some(context_id));
    let remember_us = remember_started.elapsed().as_micros();
    let publish_started = Instant::now();
    publish_window_list_snapshot(caller);
    let publish_us = publish_started.elapsed().as_micros();
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "windows.switch_window",
        "previous_context_id": previous_context,
        "selected_context_id": context_id,
        "context_count": contexts.len(),
        "context_list_us": context_list_us,
        "resolve_us": resolve_us,
        "context_select_us": context_select_us,
        "remember_us": remember_us,
        "publish_us": publish_us,
        "total_us": total_started.elapsed().as_micros(),
    }));
    Ok(WindowAck {
        ok: true,
        id: Some(context_id.to_string()),
    })
}

#[allow(clippy::needless_pass_by_value)] // Plugin command dispatch passes owned direction from deserialized request
fn cycle_window(
    caller: &impl HostRuntimeApi,
    direction: WindowCycleDirection,
    last_selected_by_client: &LastSelectedByClient,
) -> Result<WindowAck, String> {
    let total_started = Instant::now();
    let list_started = Instant::now();
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let context_list_us = list_started.elapsed().as_micros();
    let order_started = Instant::now();
    let contexts = order_contexts_for_navigation(caller, contexts)?;
    let order_us = order_started.elapsed().as_micros();
    if contexts.len() < 2 {
        return Err("no alternate window available".to_string());
    }
    let resolve_started = Instant::now();
    let current_context = resolve_effective_current_context_with_contexts(caller, &contexts)?
        .unwrap_or(contexts[0].id);
    let current_index = contexts
        .iter()
        .position(|context| context.id == current_context)
        .unwrap_or(0);
    let target_id = match direction {
        WindowCycleDirection::Next => contexts[(current_index + 1) % contexts.len()].id,
        WindowCycleDirection::Previous => {
            contexts[(current_index + contexts.len() - 1) % contexts.len()].id
        }
        WindowCycleDirection::Last => {
            let remembered_by_client = caller.current_client().ok().and_then(|client| {
                last_selected_by_client
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&client.id).copied())
            });
            let remembered = remembered_by_client
                .or_else(|| {
                    get_stored_context_id(caller, PREVIOUS_WINDOW_CONTEXT_KEY)
                        .ok()
                        .flatten()
                })
                .ok_or_else(|| "no previously active window available".to_string())?;
            if !contexts.iter().any(|context| context.id == remembered) {
                return Err("no previously active window available".to_string());
            }
            if remembered == current_context {
                return Err("no previously active window available".to_string());
            }
            remembered
        }
    };
    let resolve_us = resolve_started.elapsed().as_micros();
    emit_attach_phase_timing(&serde_json::json!({
        "phase": "windows.cycle_window",
        "direction": format!("{direction:?}"),
        "current_context_id": current_context,
        "target_context_id": target_id,
        "context_count": contexts.len(),
        "context_list_us": context_list_us,
        "order_us": order_us,
        "resolve_us": resolve_us,
        "pre_switch_us": total_started.elapsed().as_micros(),
    }));
    switch_window_with_contexts(
        caller,
        &ContextSelector::ById(target_id),
        last_selected_by_client,
        &contexts,
        context_list_us,
        total_started,
    )
}

fn goto_window_by_index(
    caller: &impl HostRuntimeApi,
    index: usize,
    last_selected_by_client: &LastSelectedByClient,
) -> Result<WindowAck, String> {
    if index == 0 {
        return Err("window index must be 1 or greater".to_string());
    }
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let contexts = order_contexts_for_navigation(caller, contexts)?;
    if contexts.is_empty() {
        return Err("no windows available".to_string());
    }
    let zero_based = index - 1;
    if zero_based >= contexts.len() {
        return Err(format!(
            "window index {index} out of range (have {} window{})",
            contexts.len(),
            if contexts.len() == 1 { "" } else { "s" }
        ));
    }
    let target_id = contexts[zero_based].id;
    switch_window(
        caller,
        ContextSelector::ById(target_id),
        last_selected_by_client,
    )
}

fn close_current_window(
    caller: &impl HostRuntimeApi,
    last_selected_by_client: &LastSelectedByClient,
) -> Result<WindowAck, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    let contexts = order_contexts_for_navigation(caller, contexts)?;
    let current_id = resolve_effective_current_context_with_contexts(caller, &contexts)?
        .ok_or_else(|| "no current window to close".to_string())?;

    // If there is another window to switch to, do so before closing.
    if contexts.len() > 1 {
        let current_index = contexts
            .iter()
            .position(|context| context.id == current_id)
            .unwrap_or(0);
        // Switch to the next window (wrapping), or previous if we are at the end.
        let fallback_index = if current_index + 1 < contexts.len() {
            current_index + 1
        } else {
            current_index.saturating_sub(1)
        };
        let fallback_id = contexts[fallback_index].id;
        let _ = switch_window(
            caller,
            ContextSelector::ById(fallback_id),
            last_selected_by_client,
        );
    }

    caller
        .context_close(&ContextCloseRequest {
            selector: ContextSelector::ById(current_id),
            force: false,
        })
        .map_err(|error| error.to_string())?;

    publish_window_list_snapshot(caller);
    Ok(WindowAck {
        ok: true,
        id: Some(current_id.to_string()),
    })
}

fn resolve_context_id_from_contexts(
    contexts: &[domain_ipc::ContextSummary],
    selector: &ContextSelector,
) -> Result<Uuid, String> {
    contexts
        .iter()
        .find(|context| match selector {
            ContextSelector::ById(id) => context.id == *id,
            ContextSelector::ByName(name) => context.name.as_deref() == Some(name.as_str()),
        })
        .map(|context| context.id)
        .ok_or_else(|| "target context not found".to_string())
}

fn resolve_effective_current_context(caller: &impl HostRuntimeApi) -> Result<Option<Uuid>, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    resolve_effective_current_context_with_contexts(caller, &contexts)
}

fn resolve_effective_current_context_with_contexts(
    caller: &impl HostRuntimeApi,
    contexts: &[domain_ipc::ContextSummary],
) -> Result<Option<Uuid>, String> {
    let current = caller
        .context_current()
        .map_err(|error| error.to_string())?
        .context
        .map(|context| context.id)
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    if current.is_some() {
        return Ok(current);
    }
    let stored_active = get_stored_context_id(caller, ACTIVE_WINDOW_CONTEXT_KEY)?
        .filter(|id| contexts.iter().any(|context| context.id == *id));
    Ok(stored_active)
}

fn get_stored_context_id(caller: &impl HostRuntimeApi, key: &str) -> Result<Option<Uuid>, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: key.to_string(),
        })
        .map_err(|error| error.to_string())?;
    let Some(value) = response.value else {
        return Ok(None);
    };
    let text = String::from_utf8(value).map_err(|error| error.to_string())?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let id = Uuid::parse_str(text.trim()).map_err(|error| error.to_string())?;
    Ok(Some(id))
}

fn set_stored_context_id(
    caller: &impl HostRuntimeApi,
    key: &str,
    context_id: Option<Uuid>,
) -> Result<(), String> {
    let value = context_id.map_or_else(Vec::new, |id| id.to_string().into_bytes());
    caller
        .storage_set(&StorageSetRequest {
            key: key.to_string(),
            value,
        })
        .map_err(|error| error.to_string())
}

fn order_contexts_for_navigation(
    caller: &impl HostRuntimeApi,
    contexts: Vec<domain_ipc::ContextSummary>,
) -> Result<Vec<domain_ipc::ContextSummary>, String> {
    let order_ids = resolve_window_order_ids(caller, &contexts)?;
    let mut by_id = contexts
        .into_iter()
        .map(|context| (context.id, context))
        .collect::<BTreeMap<_, _>>();
    Ok(order_ids
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect())
}

fn resolve_window_order_ids(
    caller: &impl HostRuntimeApi,
    contexts: &[domain_ipc::ContextSummary],
) -> Result<Vec<Uuid>, String> {
    let mut order_ids = get_stored_window_order_ids(caller)?;
    if order_ids.is_empty() && !contexts.is_empty() {
        order_ids = contexts.iter().map(|context| context.id).collect();
        order_ids.sort_by_key(uuid::Uuid::as_u128);
        set_stored_window_order_ids(caller, &order_ids)?;
        return Ok(order_ids);
    }

    let mut changed = false;

    let mut seen = HashSet::new();
    let mut deduped = Vec::with_capacity(order_ids.len());
    for id in order_ids {
        if seen.insert(id) {
            deduped.push(id);
        } else {
            changed = true;
        }
    }
    order_ids = deduped;

    let context_ids = contexts
        .iter()
        .map(|context| context.id)
        .collect::<HashSet<_>>();
    let retained_len = order_ids.len();
    order_ids.retain(|id| context_ids.contains(id));
    if order_ids.len() != retained_len {
        changed = true;
    }

    if changed {
        set_stored_window_order_ids(caller, &order_ids)?;
    }

    let mut known_ids = order_ids.iter().copied().collect::<HashSet<_>>();
    // Append missing contexts only in the returned projection, never
    // in persisted storage. `contexts` is MRU-first, so persisting
    // this fallback would reintroduce tab-order shuffling on every
    // selection. Creation/close event handlers own durable order
    // mutations; this branch is just a display safety net for contexts
    // that predate the windows order stream.
    let mut missing = contexts
        .iter()
        .filter(|context| !known_ids.contains(&context.id))
        .map(|context| context.id)
        .collect::<Vec<_>>();
    missing.sort_by_key(uuid::Uuid::as_u128);
    for id in missing {
        if known_ids.insert(id) {
            order_ids.push(id);
        }
    }

    Ok(order_ids)
}

fn get_stored_window_order_ids(caller: &impl HostRuntimeApi) -> Result<Vec<Uuid>, String> {
    let response = caller
        .storage_get(&StorageGetRequest {
            key: WINDOW_ORDER_KEY.to_string(),
        })
        .map_err(|error| error.to_string())?;
    let Some(value) = response.value else {
        return Ok(Vec::new());
    };
    if value.is_empty() {
        return Ok(Vec::new());
    }

    let raw = serde_json::from_slice::<Vec<String>>(&value)
        .map_err(|error| format!("failed parsing stored window order: {error}"))?;
    raw.into_iter()
        .map(|entry| {
            Uuid::parse_str(entry.trim()).map_err(|error| {
                format!("failed parsing stored window order UUID '{entry}': {error}")
            })
        })
        .collect()
}

fn set_stored_window_order_ids(
    caller: &impl HostRuntimeApi,
    order_ids: &[Uuid],
) -> Result<(), String> {
    let payload = order_ids.iter().map(Uuid::to_string).collect::<Vec<_>>();
    let value = serde_json::to_vec(&payload)
        .map_err(|error| format!("failed encoding stored window order: {error}"))?;
    caller
        .storage_set(&StorageSetRequest {
            key: WINDOW_ORDER_KEY.to_string(),
            value,
        })
        .map_err(|error| error.to_string())
}

// ── Typed service handles ────────────────────────────────────────────
//
// The BPDL-generated `WindowsCommandsService` and `WindowsStateService`
// traits are implemented on dedicated handle structs that carry an owned
// `TypedServiceCaller`. The byte-encoded `invoke_service` path remains
// for consumers that don't use typed dispatch; both paths share the
// same underlying sync helpers and the same `LastSelectedByClient` map,
// so behaviour is identical between routes.

/// Shared state backing both the typed commands handle and the byte-
/// encoded dispatch path.
#[derive(Clone)]
struct WindowsSharedState {
    caller: Arc<TypedServiceCaller>,
    last_selected_by_client: LastSelectedByClient,
}

/// Typed implementation of [`WindowsCommandsService`]. Wraps a
/// [`TypedServiceCaller`] so trait methods can drive host calls
/// directly without a per-call [`NativeServiceContext`].
pub struct WindowsCommandsHandle {
    shared: WindowsSharedState,
}

impl WindowsCommandsHandle {
    const fn new(shared: WindowsSharedState) -> Self {
        Self { shared }
    }
}

/// Typed implementation of [`WindowsStateService`]. Reads live pane
/// state through the same host runtime the byte path uses.
pub struct WindowsStateHandle {
    shared: WindowsSharedState,
}

impl WindowsStateHandle {
    const fn new(shared: WindowsSharedState) -> Self {
        Self { shared }
    }
}

/// Convert a typed [`Selector`] to the IPC [`domain_ipc::SessionSelector`]
/// used by the byte-encoded host API. Prefers `id` when both are set.
fn selector_to_session(selector: &Selector) -> Option<domain_ipc::SessionSelector> {
    if let Some(id) = selector.id {
        return Some(domain_ipc::SessionSelector::ById(id));
    }
    selector
        .name
        .as_ref()
        .map(|name| domain_ipc::SessionSelector::ByName(name.clone()))
}

/// Convert a typed [`Selector`] to the IPC [`domain_ipc::PaneSelector`].
/// The BPDL selector has `id` / `name`; panes don't currently accept
/// a name selector on the host side, so a bare `name` falls back to
/// the active pane. Consumers that need index-based selection can
/// extend the BPDL `selector` record later.
/// Convert a typed [`Selector`] to the IPC [`domain_ipc::PaneSelector`].
/// Precedence: `id` → `index` → `name` → active. Name-based pane
/// selection has no direct IPC equivalent today, so a bare `name`
/// falls back to the active pane.
#[allow(clippy::option_if_let_else)] // Chained `if let` is clearer than nested `map_or` here.
const fn selector_to_pane(selector: &Selector) -> domain_ipc::PaneSelector {
    if let Some(id) = selector.id {
        domain_ipc::PaneSelector::ById(id)
    } else if let Some(index) = selector.index {
        domain_ipc::PaneSelector::ByIndex(index)
    } else {
        domain_ipc::PaneSelector::Active
    }
}

const fn pane_direction_to_split(direction: PaneDirection) -> domain_ipc::PaneSplitDirection {
    // The BPDL enum covers split *and* focus directions; only Horizontal
    // and Vertical are meaningful for splitting. Anything else folds to
    // Horizontal as the safest default — the trait's `split_pane` caller
    // is expected to pick Horizontal/Vertical explicitly.
    match direction {
        PaneDirection::Vertical => domain_ipc::PaneSplitDirection::Vertical,
        PaneDirection::Horizontal
        | PaneDirection::Left
        | PaneDirection::Right
        | PaneDirection::Up
        | PaneDirection::Down => domain_ipc::PaneSplitDirection::Horizontal,
    }
}

const fn pane_direction_to_focus(
    direction: PaneDirection,
) -> Option<domain_ipc::PaneFocusDirection> {
    match direction {
        // Only Next/Prev make sense at the IPC level today. The rest
        // map to "no direction hint" so the host focuses the targeted
        // pane explicitly.
        PaneDirection::Horizontal | PaneDirection::Vertical => None,
        PaneDirection::Right | PaneDirection::Down => Some(domain_ipc::PaneFocusDirection::Next),
        PaneDirection::Left | PaneDirection::Up => Some(domain_ipc::PaneFocusDirection::Prev),
    }
}

#[allow(clippy::needless_pass_by_value)] // Used as a fn-pointer in `.map_err(...)`; ref-taking would require closures.
fn map_host_error<E: ToString>(err: E) -> PaneMutationError {
    PaneMutationError::Failed {
        reason: err.to_string(),
    }
}

impl WindowsCommandsService for WindowsCommandsHandle {
    fn focus_pane<'a>(
        &'a self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), FocusError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneFocusRequest {
                session: None,
                target: Some(domain_ipc::PaneSelector::ById(id)),
                direction: None,
            };
            caller
                .pane_focus(&request)
                .map(|_| ())
                .map_err(|error| FocusError::FocusDenied {
                    reason: error.to_string(),
                })
        })
    }

    fn close_pane<'a>(
        &'a self,
        id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<(), CloseError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneCloseRequest {
                session: None,
                target: Some(domain_ipc::PaneSelector::ById(id)),
            };
            caller
                .pane_close(&request)
                .map(|_| ())
                .map_err(|error| CloseError::CloseDenied {
                    reason: error.to_string(),
                })
        })
    }

    fn focus_pane_by_selector<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let pane_selector = selector_to_pane(&target);
            let request = domain_ipc::PaneFocusRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: Some(pane_selector),
                direction: None,
            };
            caller
                .pane_focus(&request)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.id),
                })
                .map_err(map_host_error)
        })
    }

    fn close_pane_by_selector<'a>(
        &'a self,
        session: Option<Selector>,
        target: Selector,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let pane_selector = selector_to_pane(&target);
            let request = domain_ipc::PaneCloseRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: Some(pane_selector),
            };
            caller
                .pane_close(&request)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.id),
                })
                .map_err(map_host_error)
        })
    }

    fn close_active_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneCloseRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: None,
            };
            caller
                .pane_close(&request)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.id),
                })
                .map_err(map_host_error)
        })
    }

    fn focus_pane_in_direction<'a>(
        &'a self,
        session: Option<Selector>,
        direction: PaneDirection,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let Some(focus_dir) = pane_direction_to_focus(direction) else {
                return Err(PaneMutationError::InvalidArgument {
                    reason: "direction must be Next/Prev (Horizontal/Vertical aren't meaningful)"
                        .into(),
                });
            };
            let request = domain_ipc::PaneFocusRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: None,
                direction: Some(focus_dir),
            };
            caller
                .pane_focus(&request)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.id),
                })
                .map_err(map_host_error)
        })
    }

    fn split_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        direction: PaneDirection,
        _ratio_pct: Option<u32>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneSplitRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: target.as_ref().map(selector_to_pane),
                direction: pane_direction_to_split(direction),
            };
            caller
                .pane_split(&request)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.id),
                })
                .map_err(map_host_error)
        })
    }

    fn launch_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        direction: PaneDirection,
        name: Option<String>,
        program: String,
        args: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneLaunchRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: target.as_ref().map(selector_to_pane),
                direction: pane_direction_to_split(direction),
                name,
                command: domain_ipc::PaneLaunchCommand {
                    program,
                    args,
                    cwd: None,
                    env: BTreeMap::new(),
                },
            };
            caller
                .pane_launch(&request)
                .map(|response| PaneAck {
                    ok: true,
                    pane_id: Some(response.id),
                })
                .map_err(map_host_error)
        })
    }

    fn resize_pane<'a>(
        &'a self,
        session: Option<Selector>,
        target: Option<Selector>,
        delta: i16,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneResizeRequest {
                session: session.as_ref().and_then(selector_to_session),
                target: target.as_ref().map(selector_to_pane),
                delta,
            };
            caller
                .pane_resize(&request)
                .map(|_| PaneAck {
                    ok: true,
                    pane_id: None,
                })
                .map_err(map_host_error)
        })
    }

    fn zoom_pane<'a>(
        &'a self,
        session: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneZoomAck, PaneMutationError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneZoomRequest {
                session: session.as_ref().and_then(selector_to_session),
            };
            caller
                .pane_zoom(&request)
                .map(|response| PaneZoomAck {
                    pane_id: response.pane_id,
                    zoomed: response.zoomed,
                })
                .map_err(map_host_error)
        })
    }

    fn restart_pane<'a>(
        &'a self,
        _session: Option<Selector>,
        _target: Option<Selector>,
    ) -> Pin<Box<dyn Future<Output = Result<PaneAck, PaneMutationError>> + Send + 'a>> {
        Box::pin(async move {
            Err(PaneMutationError::Failed {
                reason: "restart-pane typed command is not wired to a host primitive yet".into(),
            })
        })
    }

    fn new_window<'a>(
        &'a self,
        name: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<WindowAck, WindowError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            create_window(&*caller, name).map_err(|reason| WindowError::Failed { reason })
        })
    }

    fn kill_window<'a>(
        &'a self,
        target: String,
        force_local: bool,
    ) -> Pin<Box<dyn Future<Output = Result<WindowAck, WindowError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let selector =
                parse_selector(&target).map_err(|reason| WindowError::Failed { reason })?;
            kill_window(&*caller, selector, force_local)
                .map_err(|reason| WindowError::Failed { reason })
        })
    }

    fn kill_all_windows<'a>(
        &'a self,
        force_local: bool,
    ) -> Pin<Box<dyn Future<Output = Result<WindowAck, WindowError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            kill_all_windows(&*caller, force_local).map_err(|reason| WindowError::Failed { reason })
        })
    }

    fn switch_window<'a>(
        &'a self,
        target: String,
    ) -> Pin<Box<dyn Future<Output = Result<WindowAck, WindowError>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        let last_selected = self.shared.last_selected_by_client.clone();
        Box::pin(async move {
            let selector =
                parse_selector(&target).map_err(|reason| WindowError::Failed { reason })?;
            switch_window(&*caller, selector, &last_selected)
                .map_err(|reason| WindowError::Failed { reason })
        })
    }
}

impl WindowsStateService for WindowsStateHandle {
    fn pane_state<'a>(
        &'a self,
        _id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<PaneState>> + Send + 'a>> {
        // Pane-level state hasn't been wired yet; return `None` for now
        // and revisit when the scene surfaces enough for the plugin to
        // materialize a full `PaneState` without the host-runtime API
        // exposing pane metadata.
        Box::pin(async move { None })
    }

    fn focused_pane<'a>(
        &'a self,
        _session: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<Uuid>> + Send + 'a>> {
        Box::pin(async move { None })
    }

    fn zoomed_pane<'a>(
        &'a self,
        _session: Uuid,
    ) -> Pin<Box<dyn Future<Output = Option<Uuid>> + Send + 'a>> {
        Box::pin(async move { None })
    }

    fn list_panes<'a>(
        &'a self,
        session: Uuid,
    ) -> Pin<Box<dyn Future<Output = Vec<PaneState>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move {
            let request = domain_ipc::PaneListRequest {
                session: Some(domain_ipc::SessionSelector::ById(session)),
            };
            let Ok(response) = caller.pane_list(&request) else {
                return Vec::new();
            };
            response
                .panes
                .into_iter()
                .map(|pane| PaneState {
                    id: pane.id,
                    session_id: session,
                    focused: pane.focused,
                    zoomed: false,
                    name: pane.name,
                    status: windows_state::PaneStatus::default(),
                })
                .collect()
        })
    }

    fn list_windows<'a>(
        &'a self,
        session: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Vec<WindowEntry>> + Send + 'a>> {
        let caller = Arc::clone(&self.shared.caller);
        Box::pin(async move { list_windows(&*caller, session.as_deref()).unwrap_or_default() })
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)] // Test helper; owned selector from deserialized request
fn resolve_session_id(
    caller: &impl HostRuntimeApi,
    selector: ContextSelector,
) -> Result<Uuid, String> {
    let contexts = caller
        .context_list()
        .map_err(|error| error.to_string())?
        .contexts;
    resolve_context_id_from_contexts(&contexts, &selector)
}

fn parse_selector(value: &str) -> Result<ContextSelector, String> {
    if let Ok(id) = Uuid::parse_str(value) {
        return Ok(ContextSelector::ById(id));
    }
    if value.trim().is_empty() {
        return Err("target must not be empty".to_string());
    }
    Ok(ContextSelector::ByName(value.to_string()))
}

fn option_value(arguments: &[String], long_name: &str) -> Option<String> {
    let long_flag = format!("--{long_name}");
    arguments
        .windows(2)
        .find_map(|chunk| (chunk[0] == long_flag).then(|| chunk[1].clone()))
}

fn has_flag(arguments: &[String], long_name: &str) -> bool {
    let long_flag = format!("--{long_name}");
    arguments.iter().any(|argument| argument == &long_flag)
}

fn positional_value(arguments: &[String]) -> Option<String> {
    arguments
        .iter()
        .find(|argument| !argument.starts_with('-'))
        .cloned()
}

/// Parse a `--direction` argument value from a keybinding-dispatched
/// plugin command into the `PaneDirection` enum understood by the
/// pane-runtime service requests.
///
/// `next` folds to `Right` and `prev`/`previous` fold to `Left` so
/// that `pane_direction_to_focus` emits the correct `Next`/`Prev`
/// mapping at the IPC boundary.
fn parse_pane_direction_arg(value: &str) -> Result<PaneDirection, String> {
    match value.to_ascii_lowercase().as_str() {
        "horizontal" => Ok(PaneDirection::Horizontal),
        "vertical" => Ok(PaneDirection::Vertical),
        "left" | "prev" | "previous" => Ok(PaneDirection::Left),
        "right" | "next" => Ok(PaneDirection::Right),
        "up" => Ok(PaneDirection::Up),
        "down" => Ok(PaneDirection::Down),
        other => Err(format!("unknown direction '{other}'")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ListWindowsArgs {
    session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct NewWindowArgs {
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillWindowArgs {
    target: String,
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KillAllWindowsArgs {
    force_local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SwitchWindowArgs {
    target: String,
}

/// Byte-wire envelope for `windows-commands/focus-pane`. The BPDL
/// trait's `focus_pane(id: uuid)` parameters serialize as a JSON
/// object with a single `id` field at the wire boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusPaneArgs {
    id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClosePaneArgs {
    id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusPaneBySelectorArgs {
    #[serde(default)]
    session: Option<Selector>,
    target: Selector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClosePaneBySelectorArgs {
    #[serde(default)]
    session: Option<Selector>,
    target: Selector,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CloseActivePaneArgs {
    #[serde(default)]
    session: Option<Selector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FocusPaneInDirectionArgs {
    #[serde(default)]
    session: Option<Selector>,
    direction: PaneDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SplitPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
    direction: PaneDirection,
    #[serde(default)]
    ratio_pct: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LaunchPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
    direction: PaneDirection,
    #[serde(default)]
    name: Option<String>,
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResizePaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
    delta: i16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ZoomPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RestartPaneArgs {
    #[serde(default)]
    session: Option<Selector>,
    #[serde(default)]
    target: Option<Selector>,
}

bmux_plugin_sdk::export_plugin!(WindowsPlugin, include_str!("../plugin.toml"));

// Compile-time guards: ensure the string literals used in `route_service!`
// and `plugin.toml` stay in sync with the BPDL-declared interface ids.
// Runtime assertion (executed once at the top of the test suite) that
// the BPDL-generated interface ids exactly match the canonical strings
// the plugin manifest and typed-service dispatch expect. A regression
// in either side will surface immediately.
#[cfg(test)]
#[test]
fn interface_ids_match_bpdl_constants() {
    assert_eq!(windows_state::INTERFACE_ID.as_str(), "windows-state");
    assert_eq!(windows_commands::INTERFACE_ID.as_str(), "windows-commands");
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin::ServiceCaller;
    use bmux_plugin_sdk::{
        ApiVersion, HostConnectionInfo, HostKernelBridge, HostMetadata, HostScope,
        NativeServiceContext, ProviderId, RegisteredService, ServiceKind, ServiceRequest,
        decode_service_message, encode_service_message,
    };
    use domain_ipc::{
        ContextCloseRequest, ContextCreateRequest, ContextListResponse, ContextSelectRequest,
        ContextSelectResponse, ContextSelector as SessionSelector,
        ContextSummary as SessionSummary,
    };
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeRequest {
        payload: Vec<u8>,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct BridgeResponse {
        payload: Vec<u8>,
    }

    #[allow(clippy::too_many_lines)]
    unsafe extern "C" fn service_test_kernel_bridge(
        input_ptr: *const u8,
        input_len: usize,
        output_ptr: *mut u8,
        output_capacity: usize,
        output_len: *mut usize,
    ) -> i32 {
        let input = unsafe { std::slice::from_raw_parts(input_ptr, input_len) };
        let bridge_request: BridgeRequest = match decode_service_message(input) {
            Ok(request) => request,
            Err(_) => return 1,
        };
        let _request: bmux_ipc::Request = match bmux_ipc::decode(&bridge_request.payload) {
            Ok(request) => request,
            Err(_) => return 1,
        };

        let response = bmux_ipc::Response::Err(bmux_ipc::ErrorResponse {
            code: bmux_ipc::ErrorCode::InvalidRequest,
            message: "unsupported request in service bridge test".to_string(),
        });

        let Ok(encoded) = bmux_ipc::encode(&response) else {
            return 1;
        };
        let Ok(output) = encode_service_message(&BridgeResponse { payload: encoded }) else {
            return 1;
        };

        if output.len() > output_capacity {
            unsafe {
                *output_len = output.len();
            }
            return 4;
        }

        unsafe {
            std::ptr::copy_nonoverlapping(output.as_ptr(), output_ptr, output.len());
            *output_len = output.len();
        }
        0
    }

    /// Install a thread-local router that answers the typed cross-
    /// plugin service calls windows-plugin makes through
    /// `KernelOps`'s context/session helpers. Tests that exercise
    /// `invoke_service`-style service dispatch keep the returned
    /// guard alive for the duration of the test.
    ///
    /// The router captures `deny_close`/`deny_create` flags the tests
    /// use to simulate the contexts plugin rejecting a command.
    #[allow(
        clippy::too_many_lines,
        clippy::result_large_err,
        clippy::items_after_statements,
        clippy::redundant_clone
    )]
    fn install_context_test_router(
        deny_create: bool,
        deny_close: bool,
    ) -> bmux_plugin::test_support::TestServiceRouterGuard {
        use bmux_plugin::test_support::{TestServiceRouter, install_test_service_router};
        let router: TestServiceRouter = std::sync::Arc::new(
            move |_caller_plugin,
                  _caller_client,
                  _capability,
                  _kind,
                  interface,
                  operation,
                  payload| {
                match (interface, operation) {
                    ("contexts-state", "list-contexts") => {
                        let contexts: Vec<
                            bmux_contexts_plugin_api::contexts_state::ContextSummary,
                        > = vec![bmux_contexts_plugin_api::contexts_state::ContextSummary {
                            id: Uuid::new_v4(),
                            name: Some("alpha".to_string()),
                            attributes: BTreeMap::new(),
                        }];
                        encode_service_message(&contexts)
                    }
                    ("contexts-state", "current-context") => {
                        let context: Option<
                            bmux_contexts_plugin_api::contexts_state::ContextSummary,
                        > = Some(bmux_contexts_plugin_api::contexts_state::ContextSummary {
                            id: Uuid::new_v4(),
                            name: Some("current".to_string()),
                            attributes: BTreeMap::new(),
                        });
                        encode_service_message(&context)
                    }
                    ("contexts-commands", "create-context") => {
                        if deny_create {
                            let err: Result<
                                bmux_contexts_plugin_api::contexts_commands::ContextAck,
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                            > = Err(
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError::Failed {
                                    reason: "session policy denied for this operation".to_string(),
                                },
                            );
                            return encode_service_message(&err);
                        }
                        #[derive(Deserialize)]
                        struct Args {
                            name: Option<String>,
                            #[serde(default)]
                            #[allow(dead_code)]
                            attributes: BTreeMap<String, String>,
                        }
                        let request: Args = decode_service_message(&payload)?;
                        let name_for_deny = request.name.as_deref();
                        if name_for_deny == Some("deny") {
                            let err: Result<
                                bmux_contexts_plugin_api::contexts_commands::ContextAck,
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                            > = Err(
                                bmux_contexts_plugin_api::contexts_commands::CreateContextError::Failed {
                                    reason: "session policy denied for this operation".to_string(),
                                },
                            );
                            return encode_service_message(&err);
                        }
                        let ok: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                        > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                            id: Uuid::new_v4(),
                            session_id: None,
                        });
                        encode_service_message(&ok)
                    }
                    ("contexts-commands", "close-context") => {
                        if deny_close {
                            let err: Result<
                                bmux_contexts_plugin_api::contexts_commands::ContextAck,
                                bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                            > = Err(
                                bmux_contexts_plugin_api::contexts_commands::CloseContextError::Failed {
                                    reason: "session policy denied for this operation".to_string(),
                                },
                            );
                            return encode_service_message(&err);
                        }
                        let ok: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                        > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                            id: Uuid::new_v4(),
                            session_id: None,
                        });
                        encode_service_message(&ok)
                    }
                    ("contexts-commands", "select-context") => {
                        let ok: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::SelectContextError,
                        > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                            id: Uuid::new_v4(),
                            session_id: None,
                        });
                        encode_service_message(&ok)
                    }
                    // Storage operations for tests.
                    ("storage-query/v1", "get") => {
                        encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value: None })
                    }
                    ("storage-command/v1", "set") => encode_service_message(&()),
                    _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                        operation: "windows_test_router",
                    }),
                }
            },
        );
        install_test_service_router(router)
    }

    fn service_test_context(
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
        capability: &str,
        kind: ServiceKind,
    ) -> NativeServiceContext {
        let host_services = vec![
            RegisteredService {
                capability: HostScope::new("bmux.contexts.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "context-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.contexts.write").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "context-command/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.clients.read").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "client-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Query,
                interface_id: "storage-query/v1".to_string(),
                provider: ProviderId::Host,
            },
            RegisteredService {
                capability: HostScope::new("bmux.storage").expect("capability should parse"),
                kind: ServiceKind::Command,
                interface_id: "storage-command/v1".to_string(),
                provider: ProviderId::Host,
            },
        ];

        NativeServiceContext {
            plugin_id: "bmux.windows".to_string(),
            request: ServiceRequest {
                caller_plugin_id: "test.caller".to_string(),
                service: RegisteredService {
                    capability: HostScope::new(capability).expect("capability should parse"),
                    kind,
                    interface_id: interface_id.to_string(),
                    provider: ProviderId::Plugin("bmux.windows".to_string()),
                },
                operation: operation.to_string(),
                payload,
            },
            required_capabilities: vec![
                "bmux.commands".to_string(),
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            provided_capabilities: vec![
                "bmux.windows.read".to_string(),
                "bmux.windows.write".to_string(),
            ],
            services: host_services,
            available_capabilities: vec![
                "bmux.contexts.read".to_string(),
                "bmux.contexts.write".to_string(),
                "bmux.clients.read".to_string(),
                "bmux.storage".to_string(),
            ],
            enabled_plugins: vec!["bmux.windows".to_string()],
            plugin_search_roots: vec!["/plugins".to_string()],
            host: HostMetadata {
                product_name: "bmux".to_string(),
                product_version: "0.1.0".to_string(),
                plugin_api_version: ApiVersion::new(1, 0),
                plugin_abi_version: ApiVersion::new(1, 0),
            },
            connection: HostConnectionInfo {
                config_dir: "/config".to_string(),
                config_dir_candidates: vec!["/config".to_string()],
                runtime_dir: "/runtime".to_string(),
                data_dir: "/data".to_string(),
                state_dir: "/state".to_string(),
            },
            settings: None,
            plugin_settings_map: std::collections::BTreeMap::new(),
            caller_client_id: None,
            host_kernel_bridge: Some(HostKernelBridge::from_fn(service_test_kernel_bridge)),
        }
    }

    struct MockHost {
        sessions: Vec<SessionSummary>,
        fail_create: bool,
        fail_kill: bool,
        fail_current_client: bool,
        current_client_id: Uuid,
        selected_session_id: Mutex<Option<Uuid>>,
        mru_context_ids: Mutex<Vec<Uuid>>,
        created_contexts: Mutex<Vec<SessionSummary>>,
        creates: Mutex<Vec<Option<String>>>,
        kills: Mutex<Vec<ContextCloseRequest>>,
        selects: Mutex<Vec<Uuid>>,
        storage: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MockHost {
        fn with_sessions(sessions: Vec<SessionSummary>) -> Self {
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                mru_context_ids: Mutex::new(sessions.iter().map(|session| session.id).collect()),
                created_contexts: Mutex::new(Vec::new()),
                sessions,
                fail_create: false,
                fail_kill: false,
                fail_current_client: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_client_query_failure() -> Self {
            let sessions = sample_sessions();
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                mru_context_ids: Mutex::new(sessions.iter().map(|session| session.id).collect()),
                created_contexts: Mutex::new(Vec::new()),
                sessions,
                fail_create: false,
                fail_kill: false,
                fail_current_client: true,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }

        fn with_failures(fail_create: bool, fail_kill: bool, _fail_pane_list: bool) -> Self {
            let sessions = sample_sessions();
            Self {
                current_client_id: Uuid::new_v4(),
                selected_session_id: Mutex::new(sessions.first().map(|session| session.id)),
                mru_context_ids: Mutex::new(sessions.iter().map(|session| session.id).collect()),
                created_contexts: Mutex::new(Vec::new()),
                sessions,
                fail_create,
                fail_kill,
                fail_current_client: false,
                creates: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
                selects: Mutex::new(Vec::new()),
                storage: Mutex::new(BTreeMap::new()),
            }
        }
    }

    impl ServiceCaller for MockHost {
        #[allow(
            clippy::too_many_lines,
            clippy::items_after_statements,
            clippy::redundant_clone
        )]
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: ServiceKind,
            interface_id: &str,
            operation: &str,
            payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            match (interface_id, operation) {
                // Typed contexts-plugin-api interfaces (the canonical
                // cross-plugin dispatch path used by KernelOps after
                // the `Request::*Context*` IPC variants were retired).
                ("contexts-state", "list-contexts") => {
                    let mru_ids = self
                        .mru_context_ids
                        .lock()
                        .expect("mru context lock should succeed")
                        .clone();
                    let mut all_sessions = self.sessions.clone();
                    all_sessions.extend(
                        self.created_contexts
                            .lock()
                            .expect("created contexts lock should succeed")
                            .iter()
                            .cloned(),
                    );
                    let mut by_id = all_sessions
                        .iter()
                        .cloned()
                        .map(|context| (context.id, context))
                        .collect::<BTreeMap<_, _>>();
                    let mut contexts = Vec::with_capacity(by_id.len());
                    for context_id in mru_ids {
                        if let Some(context) = by_id.remove(&context_id) {
                            contexts.push(context);
                        }
                    }
                    contexts.extend(by_id.into_values());
                    let typed: Vec<bmux_contexts_plugin_api::contexts_state::ContextSummary> =
                        contexts
                            .into_iter()
                            .map(
                                |c| bmux_contexts_plugin_api::contexts_state::ContextSummary {
                                    id: c.id,
                                    name: c.name,
                                    attributes: c.attributes,
                                },
                            )
                            .collect();
                    encode_service_message(&typed)
                }
                ("contexts-state", "current-context") => {
                    let current_context_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected context lock should succeed");
                    let typed: Option<bmux_contexts_plugin_api::contexts_state::ContextSummary> =
                        current_context_id
                            .and_then(|id| {
                                self.sessions.iter().find(|entry| entry.id == id).cloned()
                            })
                            .map(
                                |c| bmux_contexts_plugin_api::contexts_state::ContextSummary {
                                    id: c.id,
                                    name: c.name,
                                    attributes: c.attributes,
                                },
                            );
                    encode_service_message(&typed)
                }
                ("contexts-commands", "create-context") => {
                    if self.fail_create {
                        let err: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                        > = Err(
                            bmux_contexts_plugin_api::contexts_commands::CreateContextError::Failed {
                                reason: "mock create failure".to_string(),
                            },
                        );
                        return encode_service_message(&err);
                    }
                    #[derive(Deserialize)]
                    struct Args {
                        name: Option<String>,
                        #[serde(default)]
                        #[allow(dead_code)]
                        attributes: BTreeMap<String, String>,
                    }
                    let request: Args = decode_service_message(&payload)?;
                    self.creates
                        .lock()
                        .expect("create log lock should succeed")
                        .push(request.name.clone());
                    let created_id = Uuid::new_v4();
                    self.created_contexts
                        .lock()
                        .expect("created contexts lock should succeed")
                        .push(SessionSummary {
                            id: created_id,
                            name: request.name.clone(),
                            attributes: BTreeMap::new(),
                        });
                    {
                        let mut mru_context_ids = self
                            .mru_context_ids
                            .lock()
                            .expect("mru context lock should succeed");
                        mru_context_ids.retain(|id| *id != created_id);
                        mru_context_ids.insert(0, created_id);
                    }
                    *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed") = Some(created_id);
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::CreateContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id: created_id,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                ("contexts-commands", "close-context") => {
                    if self.fail_kill {
                        let err: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                        > = Err(
                            bmux_contexts_plugin_api::contexts_commands::CloseContextError::Failed {
                                reason: "mock kill failure".to_string(),
                            },
                        );
                        return encode_service_message(&err);
                    }
                    #[derive(Deserialize)]
                    struct SelectorPayload {
                        id: Option<Uuid>,
                        name: Option<String>,
                    }
                    #[derive(Deserialize)]
                    struct Args {
                        selector: SelectorPayload,
                        #[serde(default)]
                        force: bool,
                    }
                    let request: Args = decode_service_message(&payload)?;
                    let resolved_id = request
                        .selector
                        .id
                        .or_else(|| {
                            request.selector.name.as_ref().and_then(|name| {
                                self.sessions
                                    .iter()
                                    .find(|session| session.name.as_deref() == Some(name.as_str()))
                                    .map(|session| session.id)
                            })
                        })
                        .unwrap_or_else(Uuid::new_v4);
                    self.kills
                        .lock()
                        .expect("kill log lock should succeed")
                        .push(ContextCloseRequest {
                            selector: request
                                .selector
                                .id
                                .map(ContextSelector::ById)
                                .or_else(|| {
                                    request.selector.name.clone().map(ContextSelector::ByName)
                                })
                                .unwrap_or(ContextSelector::ById(resolved_id)),
                            force: request.force,
                        });
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::CloseContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id: resolved_id,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                ("contexts-commands", "select-context") => {
                    if self.fail_kill {
                        let err: Result<
                            bmux_contexts_plugin_api::contexts_commands::ContextAck,
                            bmux_contexts_plugin_api::contexts_commands::SelectContextError,
                        > = Err(
                            bmux_contexts_plugin_api::contexts_commands::SelectContextError::Denied {
                                reason: "mock select failure".to_string(),
                            },
                        );
                        return encode_service_message(&err);
                    }
                    #[derive(Deserialize)]
                    struct SelectorPayload {
                        id: Option<Uuid>,
                        name: Option<String>,
                    }
                    #[derive(Deserialize)]
                    struct Args {
                        selector: SelectorPayload,
                    }
                    let request: Args = decode_service_message(&payload)?;
                    let selected = match (request.selector.id, request.selector.name.as_ref()) {
                        (Some(id), _) => {
                            let exists = self.sessions.iter().any(|session| session.id == id)
                                || self
                                    .created_contexts
                                    .lock()
                                    .expect("created contexts lock should succeed")
                                    .iter()
                                    .any(|context| context.id == id);
                            if !exists {
                                return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                                    details: "mock select target not found".to_string(),
                                });
                            }
                            id
                        }
                        (None, Some(name)) => self
                            .sessions
                            .iter()
                            .find(|session| session.name.as_deref() == Some(name.as_str()))
                            .map(|session| session.id)
                            .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock select target not found".to_string(),
                            })?,
                        (None, None) => {
                            return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock select missing selector".to_string(),
                            });
                        }
                    };
                    *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed") = Some(selected);
                    {
                        let mut mru_context_ids = self
                            .mru_context_ids
                            .lock()
                            .expect("mru context lock should succeed");
                        mru_context_ids.retain(|id| *id != selected);
                        mru_context_ids.insert(0, selected);
                    }
                    self.selects
                        .lock()
                        .expect("select log lock should succeed")
                        .push(selected);
                    let ok: Result<
                        bmux_contexts_plugin_api::contexts_commands::ContextAck,
                        bmux_contexts_plugin_api::contexts_commands::SelectContextError,
                    > = Ok(bmux_contexts_plugin_api::contexts_commands::ContextAck {
                        id: selected,
                        session_id: None,
                    });
                    encode_service_message(&ok)
                }
                // Legacy context-query/v1 + context-command/v1 retained
                // for tests that still exercise those paths directly.
                ("context-query/v1", "list") => {
                    let mru_ids = self
                        .mru_context_ids
                        .lock()
                        .expect("mru context lock should succeed")
                        .clone();
                    let mut by_id = self
                        .sessions
                        .iter()
                        .cloned()
                        .map(|context| (context.id, context))
                        .collect::<BTreeMap<_, _>>();
                    let mut contexts = Vec::with_capacity(by_id.len());
                    for context_id in mru_ids {
                        if let Some(context) = by_id.remove(&context_id) {
                            contexts.push(context);
                        }
                    }
                    contexts.extend(by_id.into_values());
                    encode_service_message(&ContextListResponse { contexts })
                }
                ("context-command/v1", "create") => {
                    if self.fail_create {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock create failure".to_string(),
                        });
                    }
                    let request: ContextCreateRequest = decode_service_message(&payload)?;
                    self.creates
                        .lock()
                        .expect("create log lock should succeed")
                        .push(request.name.clone());
                    encode_service_message(&domain_ipc::ContextCreateResponse {
                        context: SessionSummary {
                            id: Uuid::new_v4(),
                            name: request.name,
                            attributes: request.attributes,
                        },
                    })
                }
                ("context-command/v1", "close") => {
                    if self.fail_kill {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock kill failure".to_string(),
                        });
                    }
                    let request: ContextCloseRequest = decode_service_message(&payload)?;
                    self.kills
                        .lock()
                        .expect("kill log lock should succeed")
                        .push(request.clone());
                    encode_service_message(&domain_ipc::ContextCloseResponse {
                        id: match request.selector {
                            SessionSelector::ById(id) => id,
                            SessionSelector::ByName(_) => Uuid::new_v4(),
                        },
                    })
                }
                ("context-command/v1", "select") => {
                    if self.fail_kill {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock select failure".to_string(),
                        });
                    }
                    let request: ContextSelectRequest = decode_service_message(&payload)?;
                    let selected = match request.selector {
                        SessionSelector::ById(id) => id,
                        SessionSelector::ByName(name) => self
                            .sessions
                            .iter()
                            .find(|session| session.name.as_deref() == Some(name.as_str()))
                            .map(|session| session.id)
                            .ok_or_else(|| bmux_plugin_sdk::PluginError::ServiceProtocol {
                                details: "mock select target not found".to_string(),
                            })?,
                    };
                    *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed") = Some(selected);
                    {
                        let mut mru_context_ids = self
                            .mru_context_ids
                            .lock()
                            .expect("mru context lock should succeed");
                        mru_context_ids.retain(|id| *id != selected);
                        mru_context_ids.insert(0, selected);
                    }
                    self.selects
                        .lock()
                        .expect("select log lock should succeed")
                        .push(selected);
                    encode_service_message(&ContextSelectResponse {
                        context: SessionSummary {
                            id: selected,
                            name: Some("selected".to_string()),
                            attributes: BTreeMap::new(),
                        },
                    })
                }
                ("context-query/v1", "current") => {
                    let current_context_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected context lock should succeed");
                    let context = current_context_id
                        .and_then(|id| self.sessions.iter().find(|entry| entry.id == id).cloned());
                    encode_service_message(&domain_ipc::ContextCurrentResponse { context })
                }
                ("client-query/v1", "current") => {
                    if self.fail_current_client {
                        return Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                            details: "mock current client failure".to_string(),
                        });
                    }
                    let selected_session_id = *self
                        .selected_session_id
                        .lock()
                        .expect("selected session lock should succeed");
                    encode_service_message(&domain_ipc::CurrentClientResponse {
                        id: self.current_client_id,
                        selected_session_id,
                        following_client_id: None,
                        following_global: false,
                    })
                }
                ("storage-query/v1", "get") => {
                    let request: StorageGetRequest = decode_service_message(&payload)?;
                    let value = self
                        .storage
                        .lock()
                        .expect("storage lock should succeed")
                        .get(&request.key)
                        .cloned();
                    encode_service_message(&bmux_plugin_sdk::StorageGetResponse { value })
                }
                ("storage-command/v1", "set") => {
                    let request: StorageSetRequest = decode_service_message(&payload)?;
                    self.storage
                        .lock()
                        .expect("storage lock should succeed")
                        .insert(request.key, request.value);
                    encode_service_message(&())
                }
                _ => Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                    operation: "mock_service",
                }),
            }
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            Err(bmux_plugin_sdk::PluginError::UnsupportedHostOperation {
                operation: "mock_execute_kernel_request",
            })
        }
    }

    fn sample_sessions() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
        ]
    }

    fn sample_sessions_three() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("gamma".to_string()),
                attributes: BTreeMap::new(),
            },
        ]
    }

    fn seed_window_order(host: &MockHost, sessions: &[SessionSummary]) {
        let ids = sessions
            .iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        set_stored_window_order_ids(host, &ids).expect("seed window order should succeed");
    }

    #[test]
    fn list_windows_projects_sessions_and_marks_first_active() {
        let sessions = sample_sessions();
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let windows = list_windows(&host, None).expect("list should succeed");

        assert_eq!(windows.len(), 2);
        assert!(windows[0].active);
        assert!(!windows[1].active);
        assert_eq!(windows[0].name, "alpha");
        assert_eq!(windows[1].name, "beta");
    }

    #[test]
    fn list_windows_filters_by_session_selector() {
        let sessions = sample_sessions();
        let beta_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);

        let by_name = list_windows(&host, Some("beta")).expect("list by name should succeed");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "beta");

        let by_id =
            list_windows(&host, Some(&beta_id.to_string())).expect("list by id should succeed");
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].id, beta_id.to_string());
    }

    #[test]
    fn list_windows_uses_tab_prefix_for_unnamed_contexts() {
        let sessions = vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: None,
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: None,
                attributes: BTreeMap::new(),
            },
        ];
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);

        let windows = list_windows(&host, None).expect("list should succeed");
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name, "tab-1");
        assert_eq!(windows[1].name, "tab-2");
    }

    #[test]
    fn resolve_session_id_finds_name_and_id() {
        let sessions = sample_sessions();
        let alpha_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);

        let resolved_name = resolve_session_id(&host, SessionSelector::ByName("alpha".to_string()))
            .expect("resolve by name should succeed");
        assert_eq!(resolved_name, alpha_id);

        let resolved_id = resolve_session_id(&host, SessionSelector::ById(alpha_id))
            .expect("resolve by id should succeed");
        assert_eq!(resolved_id, alpha_id);
    }

    #[test]
    fn parse_selector_rejects_blank_values() {
        let error = parse_selector("   ").expect_err("blank selector should fail");
        assert!(error.contains("must not be empty"));
    }

    #[test]
    fn create_window_calls_session_create() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let ack = create_window(&host, Some("dev".to_string())).expect("create should succeed");
        assert!(ack.ok);
        let created_id = ack.id.expect("create should return context id");
        let created_id = Uuid::parse_str(&created_id).expect("created id should be uuid");
        let stored_order = get_stored_window_order_ids(&host).expect("order lookup should succeed");
        assert_eq!(stored_order, vec![first_id, created_id]);
        let creates: Vec<_> = host
            .creates
            .lock()
            .expect("create log lock should succeed")
            .clone();
        assert_eq!(creates.as_slice(), &[Some("dev".to_string())]);
    }

    #[test]
    fn create_window_seeds_current_context_before_new_context() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);

        let ack = create_window(&host, None).expect("create should succeed");
        let created_id =
            Uuid::parse_str(ack.id.as_deref().expect("create should return context id"))
                .expect("created id should be uuid");

        let stored_order = get_stored_window_order_ids(&host).expect("order lookup should succeed");
        assert_eq!(stored_order, vec![first_id, created_id]);

        let windows = list_windows(&host, None).expect("list should succeed");
        let ids = windows
            .iter()
            .map(|window| window.id.as_str())
            .collect::<Vec<_>>();
        let first_text = first_id.to_string();
        let created_text = created_id.to_string();
        assert_eq!(ids[0], first_text.as_str());
        assert_eq!(ids[1], created_text.as_str());
    }

    #[test]
    fn create_window_assigns_next_tab_name_when_name_is_missing() {
        let sessions = vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("tab-1".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("tab-3".to_string()),
                attributes: BTreeMap::new(),
            },
        ];
        let host = MockHost::with_sessions(sessions);

        let ack = create_window(&host, None).expect("create should succeed");
        assert!(ack.ok);
        assert!(ack.id.is_some());
        let creates: Vec<_> = host
            .creates
            .lock()
            .expect("create log lock should succeed")
            .clone();
        assert_eq!(creates.as_slice(), &[Some("tab-2".to_string())]);
    }

    #[test]
    fn kill_all_windows_calls_kill_for_each_session() {
        let host = MockHost::with_sessions(sample_sessions());
        let ack = kill_all_windows(&host, true).expect("kill all should succeed");
        assert!(ack.ok);
        let (kill_count, all_force) = {
            let kills = host.kills.lock().expect("kill log lock should succeed");
            (kills.len(), kills.iter().all(|request| request.force))
        };
        assert_eq!(kill_count, 2);
        assert!(all_force);
    }

    #[test]
    fn kill_window_passes_selector_and_force_local() {
        let host = MockHost::with_sessions(sample_sessions());
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;

        let ack =
            kill_window(&host, SessionSelector::ById(target), true).expect("kill should succeed");
        assert!(ack.ok);
        let target_text = target.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let (kill_count, first_matches_target, first_force) = {
            let kills = host.kills.lock().expect("kill log lock should succeed");
            (
                kills.len(),
                kills.first().is_some_and(
                    |k| matches!(k.selector, SessionSelector::ById(id) if id == target),
                ),
                kills.first().is_some_and(|k| k.force),
            )
        };
        assert_eq!(kill_count, 1);
        assert!(first_matches_target);
        assert!(first_force);
    }

    #[test]
    fn switch_window_requires_target_context_to_exist() {
        let host = MockHost::with_sessions(sample_sessions());
        let last_selected_by_client = LastSelectedByClient::default();
        let error = switch_window(
            &host,
            SessionSelector::ById(Uuid::new_v4()),
            &last_selected_by_client,
        )
        .expect_err("switch should fail when context is missing");
        assert!(error.contains("not found"));
    }

    #[test]
    fn switch_window_returns_selected_session_id() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = switch_window(
            &host,
            SessionSelector::ById(target_id),
            &last_selected_by_client,
        )
        .expect("switch should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));

        let selects: Vec<_> = host
            .selects
            .lock()
            .expect("select log lock should succeed")
            .clone();
        assert_eq!(selects.as_slice(), &[target_id]);
    }

    #[test]
    fn switch_window_succeeds_when_current_client_query_fails() {
        let host = MockHost::with_client_query_failure();
        let target_id = host
            .sessions
            .get(1)
            .expect("sample sessions should include second item")
            .id;
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = switch_window(
            &host,
            SessionSelector::ById(target_id),
            &last_selected_by_client,
        )
        .expect("switch should succeed even if current client query fails");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn next_window_selects_second_session() {
        let sessions = sample_sessions();
        let target_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = cycle_window(&host, WindowCycleDirection::Next, &last_selected_by_client)
            .expect("next window should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn prev_window_selects_last_session() {
        let sessions = vec![
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: Uuid::new_v4(),
                name: Some("gamma".to_string()),
                attributes: BTreeMap::new(),
            },
        ];
        let target_id = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = cycle_window(
            &host,
            WindowCycleDirection::Previous,
            &last_selected_by_client,
        )
        .expect("previous window should succeed");
        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn cycle_window_follows_stable_order_when_mru_updates() {
        let sessions = sample_sessions_three();
        let first_id = sessions[0].id;
        let second_id = sessions[1].id;
        let third_id = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let next = cycle_window(&host, WindowCycleDirection::Next, &last_selected_by_client)
            .expect("next window should succeed");
        let second_text = second_id.to_string();
        assert_eq!(next.id.as_deref(), Some(second_text.as_str()));

        let next_again = cycle_window(&host, WindowCycleDirection::Next, &last_selected_by_client)
            .expect("second next window should succeed");
        let third_text = third_id.to_string();
        assert_eq!(next_again.id.as_deref(), Some(third_text.as_str()));

        let previous = cycle_window(
            &host,
            WindowCycleDirection::Previous,
            &last_selected_by_client,
        )
        .expect("previous window should succeed");
        assert_eq!(previous.id.as_deref(), Some(second_text.as_str()));

        let selects = host
            .selects
            .lock()
            .expect("select log lock should succeed")
            .clone();
        assert_eq!(selects, vec![second_id, third_id, second_id]);

        let stored_order = get_stored_window_order_ids(&host).expect("order lookup should succeed");
        assert_eq!(stored_order, vec![first_id, second_id, third_id]);
    }

    #[test]
    fn list_windows_keeps_stable_order_after_switches() {
        let sessions = sample_sessions_three();
        let first_id = sessions[0].id;
        let second_id = sessions[1].id;
        let third_id = sessions[2].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let _ = cycle_window(&host, WindowCycleDirection::Next, &last_selected_by_client)
            .expect("next window should succeed");

        let windows = list_windows(&host, None).expect("list should succeed");
        assert_eq!(windows.len(), 3);
        let window_ids = windows
            .iter()
            .map(|window| window.id.as_str())
            .collect::<Vec<_>>();
        let first_text = first_id.to_string();
        let second_text = second_id.to_string();
        let third_text = third_id.to_string();
        assert_eq!(
            window_ids,
            vec![
                first_text.as_str(),
                second_text.as_str(),
                third_text.as_str()
            ]
        );
        assert!(
            windows
                .iter()
                .any(|window| window.active && window.id == second_text)
        );
    }

    #[test]
    fn empty_window_order_initializes_to_deterministic_order_not_mru() {
        let first_id = Uuid::from_u128(1);
        let second_id = Uuid::from_u128(2);
        let third_id = Uuid::from_u128(3);
        let sessions = vec![
            SessionSummary {
                id: third_id,
                name: Some("gamma".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: first_id,
                name: Some("alpha".to_string()),
                attributes: BTreeMap::new(),
            },
            SessionSummary {
                id: second_id,
                name: Some("beta".to_string()),
                attributes: BTreeMap::new(),
            },
        ];
        let host = MockHost::with_sessions(sessions);
        *host
            .mru_context_ids
            .lock()
            .expect("mru context lock should succeed") = vec![third_id, second_id, first_id];

        let windows = list_windows(&host, None).expect("list should succeed");
        let ids = windows
            .iter()
            .map(|window| window.id.as_str())
            .collect::<Vec<_>>();
        let first_text = first_id.to_string();
        let second_text = second_id.to_string();
        let third_text = third_id.to_string();
        assert_eq!(
            ids,
            vec![
                first_text.as_str(),
                second_text.as_str(),
                third_text.as_str()
            ]
        );

        let stored_order = get_stored_window_order_ids(&host).expect("order lookup should succeed");
        assert_eq!(stored_order, vec![first_id, second_id, third_id]);

        *host
            .mru_context_ids
            .lock()
            .expect("mru context lock should succeed") = vec![second_id, third_id, first_id];

        let windows = list_windows(&host, None).expect("second list should succeed");
        let ids = windows
            .iter()
            .map(|window| window.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                first_text.as_str(),
                second_text.as_str(),
                third_text.as_str()
            ]
        );
    }

    #[test]
    fn last_window_requires_alternate_session() {
        let sessions = vec![SessionSummary {
            id: Uuid::new_v4(),
            name: Some("solo".to_string()),
            attributes: BTreeMap::new(),
        }];
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();
        let error = cycle_window(&host, WindowCycleDirection::Last, &last_selected_by_client)
            .expect_err("last window should require alternate session");
        assert!(error.contains("no alternate window"));
    }

    #[test]
    fn last_window_selects_recorded_previous_session() {
        let sessions = sample_sessions();
        let target_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let _ = cycle_window(&host, WindowCycleDirection::Next, &last_selected_by_client)
            .expect("next window should succeed");

        let ack = cycle_window(&host, WindowCycleDirection::Last, &last_selected_by_client)
            .expect("last window should use remembered selection");

        assert!(ack.ok);
        let target_text = target_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(target_text.as_str()));
    }

    #[test]
    fn create_window_propagates_host_error() {
        let host = MockHost::with_failures(true, false, false);
        let error = create_window(&host, Some("dev".to_string()))
            .expect_err("create should surface host failure");
        assert!(error.contains("mock create failure"), "error was: {error}");
    }

    #[test]
    fn kill_window_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let error = kill_window(&host, SessionSelector::ByName("alpha".to_string()), false)
            .expect_err("kill should surface host failure");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn kill_all_windows_propagates_host_error() {
        let host = MockHost::with_failures(false, true, false);
        let error = kill_all_windows(&host, true).expect_err("kill all should fail on host error");
        assert!(error.contains("mock kill failure"));
    }

    #[test]
    fn switch_window_propagates_context_select_error() {
        let host = MockHost::with_failures(false, true, false);
        let target = host
            .sessions
            .first()
            .expect("sample sessions should exist")
            .id;
        let last_selected_by_client = LastSelectedByClient::default();
        let error = switch_window(
            &host,
            SessionSelector::ById(target),
            &last_selected_by_client,
        )
        .expect_err("switch should fail when select fails");
        assert!(error.contains("mock select failure"), "error was: {error}");
    }

    #[test]
    fn invoke_service_new_returns_ack_with_id() {
        let _router = install_context_test_router(false, false);
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "windows-commands",
            "new-window",
            encode_service_message(&NewWindowArgs {
                name: Some("ok".to_string()),
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let ack: WindowAck = decode_service_message(&response.payload).expect("ack should decode");
        assert!(ack.ok);
        assert!(ack.id.is_some());
    }

    #[test]
    fn invoke_service_new_surfaces_denied_error() {
        let _router = install_context_test_router(false, false);
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "windows-commands",
            "new-window",
            encode_service_message(&NewWindowArgs {
                name: Some("deny".to_string()),
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected service error");
        assert_eq!(error.code, "new_failed");
        assert!(error.message.contains("session policy denied"));
    }

    #[test]
    fn invoke_service_switch_returns_ack_with_selected_id() {
        let _router = install_context_test_router(false, false);
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "windows-commands",
            "switch-window",
            encode_service_message(&SwitchWindowArgs {
                target: "alpha".to_string(),
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let ack: WindowAck = decode_service_message(&response.payload).expect("ack should decode");
        assert!(ack.ok);
        assert!(ack.id.is_some_and(|id| !id.is_empty()));
    }

    #[test]
    fn invoke_service_rejects_invalid_payload() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "windows-commands",
            "kill-window",
            vec![1, 2, 3],
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected service error");
        assert_eq!(error.code, "invalid_request");
    }

    #[test]
    fn invoke_service_kill_surfaces_denied_error() {
        let _router = install_context_test_router(false, true);
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "windows-commands",
            "kill-window",
            encode_service_message(&KillWindowArgs {
                target: "deny".to_string(),
                force_local: false,
            })
            .expect("request should encode"),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response.error.expect("expected kill failure");
        assert_eq!(error.code, "kill_failed");
        assert!(error.message.contains("session policy denied"));
    }

    #[test]
    fn invoke_service_rejects_unsupported_operation() {
        let mut plugin = WindowsPlugin::default();
        let context = service_test_context(
            "windows-commands",
            "unknown",
            Vec::new(),
            "bmux.windows.write",
            ServiceKind::Command,
        );

        let response = plugin.invoke_service(context);
        let error = response
            .error
            .expect("expected unsupported operation error");
        assert_eq!(error.code, "unsupported_service_operation");
    }

    #[test]
    fn goto_window_by_index_selects_first_context() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = goto_window_by_index(&host, 1, &last_selected_by_client)
            .expect("goto index 1 should succeed");
        assert!(ack.ok);
        let first_text = first_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(first_text.as_str()));
    }

    #[test]
    fn goto_window_by_index_selects_second_context() {
        let sessions = sample_sessions();
        let second_id = sessions[1].id;
        let host = MockHost::with_sessions(sessions.clone());
        seed_window_order(&host, &sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = goto_window_by_index(&host, 2, &last_selected_by_client)
            .expect("goto index 2 should succeed");
        assert!(ack.ok);
        let second_text = second_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(second_text.as_str()));
    }

    #[test]
    fn goto_window_by_index_rejects_zero() {
        let host = MockHost::with_sessions(sample_sessions());
        let last_selected_by_client = LastSelectedByClient::default();

        let error = goto_window_by_index(&host, 0, &last_selected_by_client)
            .expect_err("index 0 should fail");
        assert!(error.contains("1 or greater"));
    }

    #[test]
    fn goto_window_by_index_rejects_out_of_range() {
        let host = MockHost::with_sessions(sample_sessions());
        let last_selected_by_client = LastSelectedByClient::default();

        let error = goto_window_by_index(&host, 99, &last_selected_by_client)
            .expect_err("index 99 should fail");
        assert!(error.contains("out of range"));
    }

    #[test]
    fn close_current_window_closes_and_switches() {
        let sessions = sample_sessions();
        let first_id = sessions[0].id;
        let host = MockHost::with_sessions(sessions);
        let last_selected_by_client = LastSelectedByClient::default();

        let ack = close_current_window(&host, &last_selected_by_client)
            .expect("close current should succeed");
        assert!(ack.ok);
        let first_text = first_id.to_string();
        assert_eq!(ack.id.as_deref(), Some(first_text.as_str()));

        // Verify that a context select was issued (switch to fallback window)
        let has_selects = !host
            .selects
            .lock()
            .expect("select log lock should succeed")
            .is_empty();
        assert!(has_selects, "should have switched to a fallback window");

        // Verify that the current window was closed
        let (kill_count, first_kill_matches) = {
            let kills = host.kills.lock().expect("kill log lock should succeed");
            (
                kills.len(),
                kills.first().is_some_and(
                    |k| matches!(k.selector, SessionSelector::ById(id) if id == first_id),
                ),
            )
        };
        assert_eq!(kill_count, 1);
        assert!(first_kill_matches);
    }

    /// Verify that `register_typed_services` installs both typed
    /// handles (`windows-state` Query, `windows-commands` Command) in
    /// the registry and that they downcast to the generated BPDL
    /// service trait objects.
    #[test]
    fn register_typed_services_installs_both_typed_handles() {
        let plugin = WindowsPlugin::default();
        let mut registry = TypedServiceRegistry::new();
        let empty_caps: Vec<String> = Vec::new();
        let services: Vec<bmux_plugin_sdk::RegisteredService> = Vec::new();
        let settings = std::collections::BTreeMap::new();
        let host_metadata = bmux_plugin_sdk::HostMetadata {
            product_name: "test".to_string(),
            product_version: "0".to_string(),
            plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
            plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
        };
        let host_connection = bmux_plugin_sdk::HostConnectionInfo {
            config_dir: "/tmp".to_string(),
            config_dir_candidates: vec!["/tmp".to_string()],
            runtime_dir: "/tmp".to_string(),
            data_dir: "/tmp".to_string(),
            state_dir: "/tmp".to_string(),
        };
        let context = TypedServiceRegistrationContext {
            plugin_id: "bmux.windows",
            host_kernel_bridge: None,
            required_capabilities: &empty_caps,
            provided_capabilities: &empty_caps,
            services: &services,
            available_capabilities: &empty_caps,
            enabled_plugins: &empty_caps,
            plugin_search_roots: &empty_caps,
            host: &host_metadata,
            connection: &host_connection,
            plugin_settings_map: &settings,
        };
        plugin.register_typed_services(context, &mut registry);

        let read_cap = HostScope::new("bmux.windows.read").expect("read capability");
        let write_cap = HostScope::new("bmux.windows.write").expect("write capability");

        let state_handle = registry
            .get(
                &read_cap,
                ServiceKind::Query,
                windows_state::INTERFACE_ID.as_str(),
            )
            .expect("state handle registered");
        let _state = state_handle
            .provider_as_trait::<dyn WindowsStateService + Send + Sync>()
            .expect("state handle downcasts to typed trait");

        let commands_handle = registry
            .get(
                &write_cap,
                ServiceKind::Command,
                windows_commands::INTERFACE_ID.as_str(),
            )
            .expect("commands handle registered");
        let _commands = commands_handle
            .provider_as_trait::<dyn WindowsCommandsService + Send + Sync>()
            .expect("commands handle downcasts to typed trait");
    }

    /// Simulates three `ContextEvent::Created` events arriving in
    /// sequence on the contexts-events channel — exactly the stream
    /// the real subscriber receives. The expected post-state is that
    /// `windows.order` contains A, B, C in that exact order.
    #[test]
    fn append_context_to_window_order_preserves_arrival_sequence() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let b = Uuid::from_u128(0x2222_2222_2222_2222_2222_2222_2222_2222);
        let c = Uuid::from_u128(0x3333_3333_3333_3333_3333_3333_3333_3333);

        append_context_to_window_order(&host, a).expect("append A");
        append_context_to_window_order(&host, b).expect("append B");
        append_context_to_window_order(&host, c).expect("append C");

        let order = get_stored_window_order_ids(&host).expect("order readable");
        assert_eq!(order, vec![a, b, c]);
    }

    /// Duplicate `Created` events for the same id must not push
    /// duplicates into `windows.order`.
    #[test]
    fn append_context_to_window_order_is_idempotent() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(0xAAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA);

        append_context_to_window_order(&host, a).expect("first append");
        append_context_to_window_order(&host, a).expect("second append");

        let order = get_stored_window_order_ids(&host).expect("order readable");
        assert_eq!(order, vec![a]);
    }

    /// Simulates a `ContextEvent::Closed` for a middle entry. The
    /// remaining entries preserve their relative order.
    #[test]
    fn remove_context_from_window_order_preserves_surrounding_order() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);

        set_stored_window_order_ids(&host, &[a, b, c]).expect("seed order");
        remove_context_from_window_order(&host, b).expect("remove B");

        let order = get_stored_window_order_ids(&host).expect("order readable");
        assert_eq!(order, vec![a, c]);
    }

    /// Closing the currently active context also clears the
    /// `ACTIVE_WINDOW_CONTEXT_KEY` marker so stale pointers don't
    /// linger.
    #[test]
    fn remove_context_from_window_order_clears_stale_active_marker() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(42);

        set_stored_window_order_ids(&host, &[a]).expect("seed order");
        set_stored_context_id(&host, ACTIVE_WINDOW_CONTEXT_KEY, Some(a)).expect("set active");
        remove_context_from_window_order(&host, a).expect("remove A");

        let active =
            get_stored_context_id(&host, ACTIVE_WINDOW_CONTEXT_KEY).expect("active readable");
        assert!(active.is_none());
    }

    /// `Selected` event promotes the target into
    /// `ACTIVE_WINDOW_CONTEXT_KEY` and demotes the previous active
    /// into `PREVIOUS_WINDOW_CONTEXT_KEY` so `last-window` still works.
    #[test]
    fn mark_context_active_promotes_previous_to_last_window_slot() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(11);
        let b = Uuid::from_u128(22);

        set_stored_context_id(&host, ACTIVE_WINDOW_CONTEXT_KEY, Some(a)).expect("seed active = A");
        mark_context_active(&host, b).expect("mark B active");

        assert_eq!(
            get_stored_context_id(&host, ACTIVE_WINDOW_CONTEXT_KEY).expect("active readable"),
            Some(b)
        );
        assert_eq!(
            get_stored_context_id(&host, PREVIOUS_WINDOW_CONTEXT_KEY).expect("previous readable"),
            Some(a)
        );
    }

    /// Re-selecting the already-active context is a no-op on the
    /// previous-window slot (no spurious swap to itself).
    #[test]
    fn mark_context_active_is_idempotent_when_already_active() {
        let host = MockHost::with_sessions(Vec::new());
        let a = Uuid::from_u128(7);

        set_stored_context_id(&host, ACTIVE_WINDOW_CONTEXT_KEY, Some(a)).expect("seed active");
        mark_context_active(&host, a).expect("re-mark active");

        assert_eq!(
            get_stored_context_id(&host, ACTIVE_WINDOW_CONTEXT_KEY).expect("active readable"),
            Some(a)
        );
        assert_eq!(
            get_stored_context_id(&host, PREVIOUS_WINDOW_CONTEXT_KEY).expect("previous readable"),
            None
        );
    }
}
