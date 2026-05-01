//! bmux pane-runtime plugin.
//!
//! Owns the concrete pane runtime and exposes it through typed services
//! plus the shared `SessionRuntimeManagerHandle` trait object.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_pane_runtime_plugin_api::{
    pane_runtime_events::{self, PaneEvent},
    pane_runtime_focus::{self, SessionFocusStateMap},
};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use std::collections::BTreeMap;

mod handlers;
mod runtime;
mod snapshot;

#[derive(Default)]
pub struct PaneRuntimePlugin;

impl RustPlugin for PaneRuntimePlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // Register the focus-state channel so subscribers (e.g.
        // the decoration plugin) can observe the focused pane per
        // session. Seeded empty — `publish_focus_state_snapshot`
        // republishes the full map on every session-mutating pane
        // command (via `handlers::publish_focus_state_snapshot`).
        bmux_plugin::global_event_bus().register_state_channel::<SessionFocusStateMap>(
            pane_runtime_focus::STATE_KIND,
            SessionFocusStateMap {
                entries: BTreeMap::new(),
                revision: 0,
            },
        );
        bmux_plugin::global_event_bus()
            .register_channel::<PaneEvent>(pane_runtime_events::EVENT_KIND);
        // Publish the initial snapshot so any already-registered
        // subscribers see the current focus state immediately (even
        // though the map is usually empty at activate time — sessions
        // are created later).
        handlers::publish_focus_state_snapshot();

        let config = bmux_plugin::global_plugin_state_registry()
            .get::<bmux_pane_runtime_plugin_api::PaneRuntimePluginConfig>()
            .and_then(|handle| handle.read().ok().map(|guard| (*guard).clone()))
            .unwrap_or_else(|| bmux_pane_runtime_plugin_api::PaneRuntimePluginConfig {
                shell: std::env::var("SHELL")
                    .ok()
                    .filter(|shell| !shell.trim().is_empty())
                    .unwrap_or_else(|| {
                        if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.to_string()
                    }),
                pane_term: "xterm-256color".to_string(),
                shell_integration_root: None,
            });
        runtime::activate_pane_runtime(config);
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        handlers::route(context)
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
    }

    fn declared_services() -> bmux_plugin_sdk::Result<Vec<bmux_plugin_sdk::PluginService>> {
        bmux_pane_runtime_plugin_api::service_declarations()
    }
}
