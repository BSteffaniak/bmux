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
#[cfg(windows)]
mod windows_environment;

#[cfg(test)]
mod retired_execution_tests;

#[cfg(test)]
mod contract_tests {
    #[test]
    fn admitted_route_contribution_is_exact_and_matches_boundary_guard() {
        use bmux_plugin_sdk::RustPlugin;
        let mut registrar = bmux_plugin_sdk::ContributionRegistrar::new();
        super::PaneRuntimePlugin
            .register_contributions(&mut registrar)
            .unwrap();
        let contributions = registrar.into_contributions();
        let [
            bmux_plugin_sdk::PluginContribution::Extension {
                extension_point,
                payload,
                ..
            },
        ] = contributions.as_slice()
        else {
            panic!("expected exact route contribution");
        };
        assert_eq!(
            extension_point,
            bmux_plugin::stream_admission::STREAM_ADMITTED_ROUTES_EXTENSION
        );
        let routes: Vec<bmux_plugin::stream_admission::StreamAdmittedRoute> =
            bmux_plugin_sdk::decode_service_message(payload).unwrap();
        assert_eq!(routes.len(), 23);
        let pointer_routes: std::collections::BTreeSet<_> = routes
            .iter()
            .filter(|route| route.interface_id == "pane-runtime-pointer-grid-v1")
            .map(|route| route.operation.as_str())
            .collect();
        assert_eq!(
            pointer_routes,
            std::collections::BTreeSet::from(["capabilities", "screen", "wait-present"])
        );
        let services = super::PaneRuntimePlugin::declared_services().unwrap();
        let unique: std::collections::BTreeSet<_> = routes
            .iter()
            .map(|route| {
                (
                    &route.capability,
                    route.kind,
                    &route.interface_id,
                    &route.operation,
                )
            })
            .collect();
        assert_eq!(unique.len(), routes.len());
        for route in routes {
            assert!(
                services.iter().any(|service| {
                    service.capability.as_str() == route.capability
                        && service.kind == route.kind
                        && service.interface_id == route.interface_id
                }),
                "approval must match an actual owned service export"
            );
            assert_ne!(route.capability, "bmux.pane_runtime.spawn.v1");
            assert!(super::handlers::execution::migrated_route(
                &route.interface_id,
                &route.operation
            ));
            assert!(route.capability.starts_with("bmux.pane_runtime."));
            assert!(
                !route.interface_id.contains("attach") && !route.interface_id.contains("readiness")
            );
            assert_eq!(
                route.kind,
                if route.interface_id == "pane-runtime-spawn-v1" {
                    bmux_plugin_sdk::ServiceKind::Query
                } else {
                    bmux_plugin_sdk::ServiceKind::Command
                }
            );
        }
    }

    #[test]
    fn pointer_grid_exports_generated_command_interface() {
        use bmux_plugin_sdk::RustPlugin as _;
        let services = super::PaneRuntimePlugin::declared_services().unwrap();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(include_str!("../plugin.toml")).unwrap();
        assert!(services.iter().any(|service| service.interface_id
            == "pane-runtime-pointer-grid-v1"
            && service.capability.as_str() == "bmux.pane_runtime.pointer_grid.v1"
            && service.kind == bmux_plugin_sdk::ServiceKind::Command));
        assert!(manifest.services.iter().any(|service| service.interface_id
            == "pane-runtime-pointer-grid-v1"
            && service.kind == bmux_plugin_sdk::ServiceKind::Command));
    }

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

    fn register_contributions(
        &self,
        registrar: &mut bmux_plugin_sdk::ContributionRegistrar,
    ) -> bmux_plugin_sdk::Result<()> {
        use bmux_plugin::stream_admission::{
            STREAM_ADMITTED_ROUTES_EXTENSION, StreamAdmittedRoute,
        };
        let interfaces = [
            (
                "bmux.pane_runtime.execution.v4",
                ServiceKind::Command,
                "pane-runtime-execution-v4",
                &[
                    "reserve",
                    "launch",
                    "reconcile",
                    "send-admitted",
                    "input-receipt",
                    "resolve",
                    "retire",
                    "new-program-session",
                    "status",
                    "screen",
                    "send",
                    "resize",
                    "terminate",
                    "wait",
                ][..],
            ),
            (
                "bmux.pane_runtime.presentation.v1",
                ServiceKind::Command,
                "pane-runtime-presentation-v1",
                &["capabilities", "resize", "capture-events", "present"][..],
            ),
            (
                "bmux.pane_runtime.presentation.wait.v1",
                ServiceKind::Command,
                "pane-runtime-presentation-wait-v1",
                &["capabilities", "wait-present"][..],
            ),
            (
                "bmux.pane_runtime.pointer_grid.v1",
                ServiceKind::Command,
                "pane-runtime-pointer-grid-v1",
                &["capabilities", "screen", "wait-present"][..],
            ),
            (
                "bmux.pane_runtime.spawn.v1",
                ServiceKind::Command,
                "pane-runtime-spawn-commands-v1",
                &["new-session", "split-shell"][..],
            ),
            (
                "bmux.pane_runtime.spawn.v1",
                ServiceKind::Query,
                "pane-runtime-spawn-v1",
                &["home-directory", "capture-source", "observe-directory"][..],
            ),
        ];
        let services = Self::declared_services()?;
        let routes: Vec<StreamAdmittedRoute> = interfaces
            .into_iter()
            .flat_map(|(capability, kind, interface_id, operations)| {
                operations.iter().map(move |operation| StreamAdmittedRoute {
                    capability: capability.into(),
                    kind,
                    interface_id: interface_id.into(),
                    operation: (*operation).into(),
                })
            })
            // Approval cannot make a modeled but unsupported service available.
            // Match the actual exports, including capability and invocation kind.
            .filter(|route| {
                services.iter().any(|service| {
                    service.capability.as_str() == route.capability
                        && service.kind == route.kind
                        && service.interface_id == route.interface_id
                })
            })
            .collect();
        registrar
            .register(bmux_plugin_sdk::PluginContribution::Extension {
                id: "pane-runtime.stream-admitted-routes".into(),
                extension_point: STREAM_ADMITTED_ROUTES_EXTENSION.into(),
                payload: bmux_plugin_sdk::encode_service_message(&routes)?,
            })
            .map_err(
                |details| bmux_plugin_sdk::PluginError::InvalidNativeServiceOutput {
                    plugin_id: "bmux.pane_runtime".into(),
                    details,
                },
            )
    }

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
        runtime::activate_managed_scope();
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
        runtime::activate_pane_runtime(config, padding_config)
            .map_err(PluginCommandError::failed)?;
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
