//! bmux recording plugin — typed recording lifecycle handlers.
//!
//! The plugin implements a typed byte-dispatch service for
//! `recording-commands` that accepts [`RecordingRequest`] payloads and
//! returns [`RecordingResponse`] payloads. Each operation reads the
//! `ManualRecordingRuntimeHandle` / `RollingRecordingRuntimeHandle`
//! out of `PluginStateRegistry`, performs the lifecycle operation
//! (start/stop/list/etc.), and returns the typed response.
//!
//! Server constructs the runtime handles at `BmuxServer::new` time
//! (with config-derived paths) and registers them; the plugin does
//! not own construction.
//!
//! This is a partial implementation — the core operations (start, stop,
//! status, list) are wired end-to-end; the rolling / cut / prune /
//! write-custom-event operations return a placeholder response and
//! will be filled in as Stage A continues.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_ipc::{RecordingProfile, RecordingStatus};
use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_recording_plugin_api::{
    ManualRecordingRuntimeHandle, RECORDING_COMMANDS_INTERFACE, RECORDING_READ, RECORDING_WRITE,
    RecordingRequest, RecordingResponse, RollingRecordingRuntimeHandle,
};

#[derive(Default)]
pub struct RecordingPlugin;

impl RustPlugin for RecordingPlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // Runtimes are constructed by `BmuxServer::new` and registered
        // into `PluginStateRegistry` there; nothing to do here today.
        // In a future migration the plugin will own construction and
        // register the handles itself.
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        bmux_plugin_sdk::route_service!(context, {
            "recording-commands", "dispatch" => |req: RecordingRequest, _ctx| {
                Ok::<RecordingResponse, ServiceResponse>(handle_recording_request(req))
            },
        })
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
        // No typed Arc<dyn Trait> surface today — recording operations
        // dispatch exclusively through the byte-service path.
    }
}

fn handle_recording_request(req: RecordingRequest) -> RecordingResponse {
    match req {
        RecordingRequest::Start {
            session_id,
            capture_input,
            name,
            profile,
            event_kinds,
        } => handle_start(session_id, capture_input, name, profile, event_kinds),
        RecordingRequest::Stop { recording_id } => handle_stop(recording_id),
        RecordingRequest::Status => handle_status(),
        RecordingRequest::List => handle_list(),
        RecordingRequest::Delete { recording_id } => handle_delete(recording_id),
        RecordingRequest::DeleteAll => handle_delete_all(),
        RecordingRequest::Prune { older_than_days } => handle_prune(older_than_days),
        // Rolling-* + Cut + CaptureTargets + WriteCustomEvent are not
        // yet implemented in the plugin — server still handles these
        // through the legacy `Request::Recording*` IPC variants. These
        // reach into server-owned config (rolling defaults, segment
        // size, recordings root) that the plugin cannot currently
        // access. Full migration is a follow-up once the plugin can
        // query server config via typed dispatch.
        RecordingRequest::WriteCustomEvent { .. }
        | RecordingRequest::Cut { .. }
        | RecordingRequest::RollingStart { .. }
        | RecordingRequest::RollingStop
        | RecordingRequest::RollingStatus
        | RecordingRequest::RollingClear { .. }
        | RecordingRequest::CaptureTargets => {
            RecordingResponse::CustomEventWritten { accepted: false }
        }
    }
}

fn handle_start(
    session_id: Option<uuid::Uuid>,
    capture_input: bool,
    name: Option<String>,
    profile: Option<RecordingProfile>,
    event_kinds: Option<Vec<bmux_ipc::RecordingEventKind>>,
) -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };

    let profile = profile.unwrap_or(RecordingProfile::Full);
    let event_kinds = event_kinds.unwrap_or_else(default_event_kinds);
    match runtime.start(session_id, capture_input, name, profile, event_kinds) {
        Ok(summary) => RecordingResponse::Started {
            recording_id: summary.id,
        },
        Err(_) => RecordingResponse::CustomEventWritten { accepted: false },
    }
}

fn handle_stop(recording_id: Option<uuid::Uuid>) -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::Stopped { recording_id: None };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::Stopped { recording_id: None };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::Stopped { recording_id: None };
    };
    match runtime.stop(recording_id) {
        Ok(summary) => RecordingResponse::Stopped {
            recording_id: Some(summary.id),
        },
        Err(_) => RecordingResponse::Stopped { recording_id: None },
    }
}

fn empty_status() -> RecordingStatus {
    RecordingStatus {
        active: None,
        queue_len: 0,
    }
}

fn handle_status() -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::Status {
            status: empty_status(),
        };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::Status {
            status: empty_status(),
        };
    };
    let Ok(runtime) = guard.0.lock() else {
        return RecordingResponse::Status {
            status: empty_status(),
        };
    };
    RecordingResponse::Status {
        status: runtime.status(),
    }
}

fn handle_list() -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::List {
            recordings: Vec::new(),
        };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::List {
            recordings: Vec::new(),
        };
    };
    let Ok(runtime) = guard.0.lock() else {
        return RecordingResponse::List {
            recordings: Vec::new(),
        };
    };
    RecordingResponse::List {
        recordings: runtime.list().unwrap_or_default(),
    }
}

fn handle_delete(recording_id: uuid::Uuid) -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::CustomEventWritten { accepted: false };
    };
    match runtime.delete(recording_id) {
        Ok(summary) => RecordingResponse::Deleted {
            recording_id: summary.id,
        },
        Err(_) => RecordingResponse::CustomEventWritten { accepted: false },
    }
}

fn handle_delete_all() -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::DeleteAll { removed_count: 0 };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::DeleteAll { removed_count: 0 };
    };
    let Ok(mut runtime) = guard.0.lock() else {
        return RecordingResponse::DeleteAll { removed_count: 0 };
    };
    RecordingResponse::DeleteAll {
        removed_count: runtime.delete_all().unwrap_or(0),
    }
}

fn handle_prune(older_than_days: Option<u64>) -> RecordingResponse {
    let Some(handle) = global_plugin_state_registry().get::<ManualRecordingRuntimeHandle>() else {
        return RecordingResponse::Pruned { pruned_count: 0 };
    };
    let Ok(guard) = handle.read() else {
        return RecordingResponse::Pruned { pruned_count: 0 };
    };
    let Ok(runtime) = guard.0.lock() else {
        return RecordingResponse::Pruned { pruned_count: 0 };
    };
    RecordingResponse::Pruned {
        pruned_count: runtime.prune(older_than_days).unwrap_or(0),
    }
}

fn default_event_kinds() -> Vec<bmux_ipc::RecordingEventKind> {
    use bmux_ipc::RecordingEventKind::{
        PaneImage, PaneInputRaw, PaneOutputRaw, ProtocolReplyRaw, RequestDone, RequestError,
        RequestStart, ServerEvent,
    };
    vec![
        PaneInputRaw,
        PaneOutputRaw,
        ProtocolReplyRaw,
        PaneImage,
        ServerEvent,
        RequestStart,
        RequestDone,
        RequestError,
    ]
}

// Silence unused-variable warnings on the constants until the full
// handler set lands.
const _KEEPS_CONSTS_ALIVE: (
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::InterfaceId,
) = (
    RECORDING_READ,
    RECORDING_WRITE,
    RECORDING_COMMANDS_INTERFACE,
);

// Placeholder to silence the unused-import lint on the rolling runtime
// handle type until the rolling-specific handlers are implemented.
#[allow(dead_code)]
fn _keep_rolling_handle_alive()
-> Option<std::sync::Arc<std::sync::RwLock<RollingRecordingRuntimeHandle>>> {
    global_plugin_state_registry().get::<RollingRecordingRuntimeHandle>()
}

bmux_plugin_sdk::export_plugin!(RecordingPlugin, include_str!("../plugin.toml"));
