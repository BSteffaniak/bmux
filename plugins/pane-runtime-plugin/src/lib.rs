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

mod application_resurrection;
mod atomic_state;
mod capture_handshake;
mod commands;
mod handlers;
pub mod observation;
mod padding;
mod padding_api;
mod private_input;
mod readiness;
mod runtime;
mod snapshot;
mod snapshot_v1_migration;
mod supervisor_protocol;

#[cfg(test)]
mod retired_execution_tests;

#[cfg(test)]
mod contract_tests {
    #[test]
    fn readiness_commands_are_discoverable() {
        use bmux_plugin_sdk::RustPlugin as _;
        let services = super::PaneRuntimePlugin::declared_services().unwrap();
        assert!(services.iter().any(
            |service| service.interface_id == "pane-runtime-readiness-v1"
                && service.capability.as_str() == "bmux.pane_runtime.write"
                && service.kind == bmux_plugin_sdk::ServiceKind::Command
        ));
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        assert!(
            manifest
                .services
                .iter()
                .any(|service| service.interface_id == "pane-runtime-readiness-v1")
        );
    }

    #[test]
    fn only_cohesive_execution_contract_is_discoverable() {
        use bmux_plugin_sdk::RustPlugin;
        let services = super::PaneRuntimePlugin::declared_services().unwrap();
        for version in [1, 2, 3] {
            assert!(services.iter().all(
                |service| service.interface_id != format!("pane-runtime-execution-v{version}")
            ));
        }
        assert!(
            services
                .iter()
                .any(|service| service.interface_id == "pane-runtime-execution-v4")
        );
    }
}

#[derive(Default)]
pub struct PaneRuntimePlugin;

impl RustPlugin for PaneRuntimePlugin {
    type Contract = bmux_pane_runtime_plugin_api::Contract;

    fn declared_services() -> bmux_plugin_sdk::Result<Vec<bmux_plugin_sdk::PluginService>> {
        // Modeled contracts do not imply runtime support: keep incomplete
        // spawn-v1 and execution-v2 services out of runtime discovery.
        let mut services =
            <Self::Contract as bmux_plugin_sdk::PluginContract>::service_declarations()?;
        services.retain(|service| {
            let capability = service.capability.as_str();
            capability != bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_SPAWN_V1.as_str()
        });
        Ok(services)
    }

    fn activate(
        &mut self,
        context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        application_resurrection::activate(&context);
        readiness::activate(&context);
        runtime::supervisor::activate(&context);
        runtime::reservations::activate(&context);
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
        handlers::pane_commands::publish_pane_lifecycle_facts();

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
                bracketed_paste: true,
                shell_integration_root: None,
            });
        let padding_config = padding::PanePaddingConfig::parse(context.settings.as_ref())
            .map_err(PluginCommandError::invalid_arguments)?;
        runtime::activate_pane_runtime(config, padding_config);
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn deactivate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        if let Some(handle) = bmux_plugin::global_plugin_state_registry()
            .get::<runtime::PanePaddingRuntimeHandle>()
            .and_then(|handle| handle.read().ok().map(|guard| guard.clone()))
        {
            handle
                .cancel_all_previews()
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        }
        bmux_presentation_state::global_presentation_fact_host_service()
            .registry()
            .remove_producer("bmux.pane_runtime");
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        commands::run(&context)?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        handlers::route(context)
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
    }
}
