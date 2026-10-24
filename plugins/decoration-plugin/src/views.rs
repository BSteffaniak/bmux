//! Caller-owned decoration runtimes. No personal scene is published globally.
#[cfg(test)]
#[path = "views_tests.rs"]
mod tests;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmux_decoration_plugin_api::{
    decoration_commands::client as commands, decoration_view_v1::client as view,
};
use bmux_plugin_sdk::{NativeServiceContext, ServiceResponse, WireEventSinkHandle};
use uuid::Uuid;

use crate::{
    DecorationEngineCommand, DecorationRuntime, SharedState, send_engine_command_blocking,
};

const MAX_VIEWS: usize = 32;
const MAX_SURFACES: usize = 1024;
const MAX_RETIRED: usize = 16;

struct Entry {
    state: SharedState,
    lifetime: Option<Uuid>,
    retired: VecDeque<Uuid>,
    revision: u64,
    closed: bool,
}

impl Entry {
    fn stop(&self) {
        if *self.state.runtime.stopped.borrow() {
            return;
        }
        // Serialize retirement with the last queued mutation before clearing retained delivery.
        let _ = send_engine_command_blocking(&self.state, |reply| {
            DecorationEngineCommand::Retire { reply }
        });
        self.state.runtime.stopped.send_replace(true);
        clear_scene(self.state.runtime.recipient.expect("view recipient"));
    }
}

pub(super) struct ViewRegistry {
    entries: Arc<Mutex<BTreeMap<Uuid, Arc<Mutex<Entry>>>>>,
}

impl Default for ViewRegistry {
    fn default() -> Self {
        let entries = Arc::new(Mutex::new(BTreeMap::<Uuid, Arc<Mutex<Entry>>>::new()));
        let weak = Arc::downgrade(&entries);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let Some(entries) = weak.upgrade() else {
                    break;
                };
                let Some(handle) =
                    bmux_plugin::global_plugin_state_registry().get::<WireEventSinkHandle>()
                else {
                    continue;
                };
                let sink = match handle.read() {
                    Ok(handle) => handle.0.clone(),
                    Err(_) => continue,
                };
                reap_disconnected(&entries, |recipient| sink.is_connected(recipient));
            }
        });
        Self { entries }
    }
}

// Transport registration is authoritative even when domain facades are absent.
// Unknown liveness must never be interpreted as disconnect or inactivity.
fn reap_disconnected(
    entries: &Mutex<BTreeMap<Uuid, Arc<Mutex<Entry>>>>,
    is_connected: impl Fn(Uuid) -> Option<bool>,
) {
    let candidates = match entries.lock() {
        Ok(entries) => entries
            .iter()
            .map(|(caller, entry)| (*caller, entry.clone()))
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    for (caller, entry) in candidates {
        if is_connected(caller) != Some(false) {
            continue;
        }
        // No global registry lock is held across an engine retirement barrier.
        let Ok(mut owner) = entry.lock() else {
            continue;
        };
        owner.closed = true;
        owner.stop();
        drop(owner);
        if let Ok(mut entries) = entries.lock()
            && entries
                .get(&caller)
                .is_some_and(|current| Arc::ptr_eq(current, &entry))
        {
            entries.remove(&caller);
        }
    }
}

impl Drop for ViewRegistry {
    fn drop(&mut self) {
        let entries = self
            .entries
            .lock()
            .map(|entries| entries.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for entry in entries {
            if let Ok(mut entry) = entry.lock() {
                entry.closed = true;
                entry.stop();
            }
        }
    }
}

pub(super) fn publish_scene(
    recipient: Uuid,
    scene: &bmux_scene_protocol::scene_protocol::DecorationScene,
) {
    let result = serde_json::to_vec(scene)
        .map_err(|error| error.to_string())
        .and_then(|payload| {
            let sink = bmux_plugin::global_plugin_state_registry()
                .get::<WireEventSinkHandle>()
                .ok_or_else(|| "targeted scene delivery unavailable".to_string())?;
            let sink = sink.read().map_err(|_| "scene sink poisoned".to_string())?;
            sink.0
                .publish_retained_to(
                    recipient,
                    bmux_scene_protocol::scene_protocol::STATE_KIND
                        .as_str()
                        .to_string(),
                    payload,
                )
                .map_err(|error| format!("{error:?}"))
        });
    if let Err(error) = result {
        tracing::warn!(%recipient, %error, "private decoration scene delivery failed; retained state remains queryable");
    }
}

fn clear_scene(recipient: Uuid) {
    if let Some(sink) = bmux_plugin::global_plugin_state_registry().get::<WireEventSinkHandle>()
        && let Ok(sink) = sink.read()
        && let Err(error) = sink.0.clear_retained_to(
            recipient,
            bmux_scene_protocol::scene_protocol::STATE_KIND.as_str(),
        )
    {
        tracing::warn!(%recipient, ?error, "private decoration scene cleanup failed");
    }
}

fn error(message: impl Into<String>) -> ServiceResponse {
    ServiceResponse::error("decoration_view_unavailable", message)
}

impl ViewRegistry {
    pub(super) fn set_shared_pane_border(&self, pane_id: Uuid, border: crate::BorderStyle) {
        let entries = self
            .entries
            .lock()
            .map(|entries| entries.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for entry in entries {
            if let Ok(entry) = entry.lock()
                && !entry.closed
            {
                let _ = send_engine_command_blocking(&entry.state, |reply| {
                    DecorationEngineCommand::SetPaneBorder {
                        pane_id,
                        border,
                        reply,
                    }
                });
            }
        }
    }

    fn entry(
        &self,
        caller: Uuid,
        shared: &SharedState,
    ) -> Result<Arc<Mutex<Entry>>, ServiceResponse> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| error("view registry poisoned"))?;
        if !entries.contains_key(&caller) {
            if entries.len() >= MAX_VIEWS {
                return Err(error("decoration view limit reached"));
            }
            let state = SharedState {
                runtime: Arc::new(DecorationRuntime {
                    recipient: Some(caller),
                    ..DecorationRuntime::default()
                }),
            };
            if let Some(handle) = shared.host_async_handle() {
                state.set_host_async_handle(handle);
            }
            entries.insert(
                caller,
                Arc::new(Mutex::new(Entry {
                    state,
                    lifetime: None,
                    retired: VecDeque::new(),
                    revision: 0,
                    closed: false,
                })),
            );
        }
        Ok(entries.get(&caller).expect("inserted view").clone())
    }

    pub(super) fn dispatch(
        &self,
        context: &NativeServiceContext,
        shared: &SharedState,
    ) -> Option<ServiceResponse> {
        let interface = context.request.service.interface_id.as_str();
        let operation = context.request.operation.as_str();
        let input_interface = bmux_decoration_plugin_api::decoration_input_hooks::INTERFACE_ID;
        let personal = interface == "decoration-view-v1"
            || (interface == "decoration-commands"
                && matches!(
                    operation,
                    "apply-view-theme-extension"
                        | "checkpoint-view-script-state"
                        | "restore-view-script-state"
                        | "discard-view-script-checkpoint"
                ))
            || interface == input_interface.as_str();
        if !personal {
            return None;
        }
        Some(
            self.dispatch_inner(context, shared)
                .unwrap_or_else(|response| response),
        )
    }

    fn dispatch_inner(
        &self,
        context: &NativeServiceContext,
        shared: &SharedState,
    ) -> Result<ServiceResponse, ServiceResponse> {
        let caller = context
            .caller_client_id
            .filter(|id| !id.is_nil())
            .ok_or_else(|| error("authenticated caller required"))?;
        let operation = context.request.operation.as_str();
        if matches!(
            operation,
            "checkpoint-view-script-state"
                | "restore-view-script-state"
                | "discard-view-script-checkpoint"
        ) && context.request.caller_plugin_id != "bmux.theme"
        {
            return Err(ServiceResponse::error(
                "checkpoint_owner_required",
                "only the theme coordinator may manage view checkpoints",
            ));
        }
        let entry = self.entry(caller, shared)?;
        let mut entry = entry
            .lock()
            .map_err(|_| error("view operation lock poisoned"))?;
        if entry.closed {
            return Err(error("view disconnected"));
        }
        let state = entry.state.clone();
        let overrides = shared.read_model(|model| model.panes.clone());
        if state.read_model(|model| model.panes != overrides) {
            send_engine_command_blocking(&state, |reply| DecorationEngineCommand::CopyOverrides {
                panes: overrides,
                reply,
            })
            .ok_or_else(|| error("view engine unavailable"))?;
        }
        if context.request.service.interface_id
            == bmux_decoration_plugin_api::decoration_input_hooks::INTERFACE_ID.as_str()
        {
            let request: bmux_decoration_plugin_api::decoration_input_hooks::client::HandleInputRequest =
                bmux_plugin_sdk::decode_service_message(&context.request.payload).map_err(|e| error(e.to_string()))?;
            if !state.read_model(|model| {
                model
                    .scene
                    .input_hooks
                    .iter()
                    .any(|hook| hook.id == request.event.hook_id)
            }) {
                return Err(error("stale or foreign decoration hook"));
            }
            return Ok(bmux_plugin_sdk::handle_service(context, |req: bmux_decoration_plugin_api::decoration_input_hooks::client::HandleInputRequest, _ctx| {
                let result = match crate::send_attach_input_soft(&state, crate::input::event_from_wire(req.event), crate::INPUT_HOOK_SOFT_WAIT) {
                    crate::SoftInputReply::Replied(result) => result,
                    _ => bmux_plugin::AttachInputResult::default(),
                };
                Ok::<_, ServiceResponse>(crate::input::result_to_wire(result))
            }));
        }
        // Decode through the generated contracts; the target runtime is never request-controlled.
        let context = context.clone();
        let routed = bmux_plugin_sdk::route_service!(context, {
            "decoration-view-v1", "update-layout" => |req: view::UpdateLayoutRequest, _ctx| {
                Ok::<_, ServiceResponse>(Self::update_layout(&mut entry, req))
            },
            "decoration-view-v1", "scene" => |_req: (), _ctx| {
                Ok::<_, ServiceResponse>(state.read_model(|model| model.scene.clone()))
            },
            "decoration-view-v1", "close" => |req: view::CloseRequest, _ctx| {
                Ok::<_, ServiceResponse>(Self::close(caller, &mut entry, req.lifetime))
            },
            "decoration-commands", "apply-view-theme-extension" => |req: commands::ApplyViewThemeExtensionRequest, ctx| {
                let candidates = req.config_dir_candidates.into_iter().map(std::path::PathBuf::from).collect::<Vec<_>>();
                Ok::<_, ServiceResponse>(crate::apply_theme_extension_toml(&state, &req.toml, &candidates, &crate::script_host_access_from_context(ctx)))
            },
            "decoration-commands", "checkpoint-view-script-state" => |_req: (), _ctx| {
                Ok::<_, ServiceResponse>(send_engine_command_blocking(&state, |reply| DecorationEngineCommand::ScriptCheckpoint { restore: false, reply }).unwrap_or_else(|| Err("view engine unavailable".into())))
            },
            "decoration-commands", "restore-view-script-state" => |_req: (), _ctx| {
                Ok::<_, ServiceResponse>(send_engine_command_blocking(&state, |reply| DecorationEngineCommand::ScriptCheckpoint { restore: true, reply }).unwrap_or_else(|| Err("view engine unavailable".into())))
            },
            "decoration-commands", "discard-view-script-checkpoint" => |_req: (), _ctx| {
                Ok::<_, ServiceResponse>(send_engine_command_blocking(&state, |reply| DecorationEngineCommand::DiscardScriptCheckpoint { reply }).unwrap_or_else(|| Err("view engine unavailable".into())))
            }
        });
        Ok(routed)
    }

    fn update_layout(entry: &mut Entry, req: view::UpdateLayoutRequest) -> Result<(), String> {
        if entry.closed {
            return Err("view disconnected".into());
        }
        if req.lifetime.is_nil() || req.surfaces.len() > MAX_SURFACES {
            return Err("invalid layout lifetime or surface bound".into());
        }
        if entry.retired.contains(&req.lifetime) {
            return Err("retired view lifetime".into());
        }
        if let Some(lifetime) = entry.lifetime
            && lifetime != req.lifetime
        {
            return Err("close current lifetime before replacement".into());
        }
        if entry.lifetime.is_some() && req.revision <= entry.revision {
            return Err("stale layout revision".into());
        }
        let snapshot = bmux_attach_layout_protocol::attach_layout_protocol::AttachLayoutSnapshot {
            surfaces: req.surfaces,
            revision: req.revision,
        };
        send_engine_command_blocking(&entry.state, |reply| {
            DecorationEngineCommand::LayoutAndFocus {
                snapshot,
                focused: req.focused_pane_id,
                reply,
            }
        })
        .ok_or("view engine unavailable")?;
        entry.lifetime = Some(req.lifetime);
        entry.revision = req.revision;
        Ok(())
    }

    fn close(caller: Uuid, entry: &mut Entry, lifetime: Uuid) -> Result<(), String> {
        if entry.lifetime != Some(lifetime) {
            return Err("stale view lifetime".into());
        }
        if entry.retired.len() >= MAX_RETIRED {
            return Err("view lifetime replacement limit reached; reconnect required".into());
        }
        entry.stop();
        entry.retired.push_back(lifetime);
        let host_async_handle = entry.state.host_async_handle();
        entry.state = SharedState {
            runtime: Arc::new(DecorationRuntime {
                recipient: Some(caller),
                ..DecorationRuntime::default()
            }),
        };
        if let Some(handle) = host_async_handle {
            entry.state.set_host_async_handle(handle);
        }
        entry.lifetime = None;
        entry.revision = 0;
        Ok(())
    }
}
