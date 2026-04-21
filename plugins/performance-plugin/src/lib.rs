//! bmux performance plugin — owns `PerformanceCaptureSettings` and
//! serves typed performance settings queries/mutations.
//!
//! The plugin implements `performance-commands::dispatch(PerformanceRequest)
//! -> PerformanceResponse` for the `bmux_performance_plugin_api`
//! surface. Server constructs the settings handle at `BmuxServer::new`
//! time (seeded from config) and registers it as a
//! `PerformanceSettingsHandle`; this plugin's handlers read/write that
//! handle and emit `PerformanceEvent::SettingsUpdated` on the plugin
//! event bus when settings change.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_performance_plugin_api::{
    EVENT_KIND, PERFORMANCE_COMMANDS_INTERFACE, PERFORMANCE_READ, PERFORMANCE_WRITE,
    PerformanceEvent, PerformanceRequest, PerformanceResponse,
};
use bmux_performance_state::{PerformanceCaptureSettings, PerformanceSettingsHandle};
use bmux_plugin::{global_event_bus, global_plugin_state_registry};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};

#[derive(Default)]
pub struct PerformancePlugin;

impl RustPlugin for PerformancePlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        global_event_bus().register_channel::<PerformanceEvent>(EVENT_KIND);
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
            "performance-commands", "dispatch" => |req: PerformanceRequest, _ctx| {
                Ok::<PerformanceResponse, ServiceResponse>(handle_request(req))
            },
        })
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
        // No typed Arc<dyn Trait> surface today — performance operations
        // dispatch exclusively through the byte-service path.
    }
}

fn handle_request(req: PerformanceRequest) -> PerformanceResponse {
    match req {
        PerformanceRequest::GetSettings => handle_get_settings(),
        PerformanceRequest::SetSettings { settings } => handle_set_settings(&settings),
    }
}

fn handle_get_settings() -> PerformanceResponse {
    let Some(handle) = global_plugin_state_registry().get::<PerformanceSettingsHandle>() else {
        return PerformanceResponse::Settings {
            settings: PerformanceCaptureSettings::default().to_runtime_settings(),
        };
    };
    let Ok(guard) = handle.read() else {
        return PerformanceResponse::Settings {
            settings: PerformanceCaptureSettings::default().to_runtime_settings(),
        };
    };
    PerformanceResponse::Settings {
        settings: guard.0.current().to_runtime_settings(),
    }
}

fn handle_set_settings(requested: &bmux_ipc::PerformanceRuntimeSettings) -> PerformanceResponse {
    let normalized_capture = PerformanceCaptureSettings::from_runtime_settings(requested);
    let normalized = normalized_capture.to_runtime_settings();

    let Some(handle) = global_plugin_state_registry().get::<PerformanceSettingsHandle>() else {
        return PerformanceResponse::Settings {
            settings: normalized,
        };
    };
    let Ok(guard) = handle.read() else {
        return PerformanceResponse::Settings {
            settings: normalized,
        };
    };
    guard.0.set(normalized_capture);

    // Emit the typed event; server's `spawn_performance_events_bridge`
    // translates this to the wire `Event::PerformanceSettingsUpdated`.
    let _ = global_event_bus().emit(
        &EVENT_KIND,
        PerformanceEvent::SettingsUpdated {
            settings: normalized.clone(),
        },
    );

    PerformanceResponse::Settings {
        settings: normalized,
    }
}

// Keep the capability/interface constants alive for consumers of the
// exported plugin binary (the symbols are referenced in the plugin's
// BPDL-free service registration via `plugin.toml`, but Rust doesn't
// see that wiring, so we touch them once in a const tuple).
const _KEEPS_CONSTS_ALIVE: (
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::InterfaceId,
) = (
    PERFORMANCE_READ,
    PERFORMANCE_WRITE,
    PERFORMANCE_COMMANDS_INTERFACE,
);

bmux_plugin_sdk::export_plugin!(PerformancePlugin, include_str!("../plugin.toml"));
