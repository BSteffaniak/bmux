//! bmux pane-runtime plugin.
//!
//! Owns the concrete pane runtime and exposes it through typed services
//! plus the shared `SessionRuntimeManagerHandle` trait object.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_pane_runtime_plugin_api::export_schema,
            crate_path: ::bmux_pane_runtime_plugin_api,
        },
        settings: "plugin.toml",
    }
}

use bmux_pane_runtime_plugin_api::{
    pane_runtime_events::{self, PaneEvent},
    pane_runtime_focus::{self, SessionFocusStateMap},
};
use bmux_plugin_sdk::author::{Declarations, Plugin};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use std::collections::BTreeMap;

mod application_resurrection;
mod atomic_state;
mod capture_handshake;
mod commands;
mod dir_order;
mod files;
mod handlers;
mod ids;
pub mod observation;
mod padding;
mod padding_api;
mod platform;
mod private_input;
mod readiness;
mod run_shared;
mod runtime;
#[cfg(feature = "simulator")]
pub use runtime::simulation;
mod snapshot;
mod snapshot_v1_migration;

#[cfg(test)]
mod retired_execution_tests;

#[cfg(test)]
mod contract_tests {
    #[test]
    fn admitted_route_contribution_is_exact_and_matches_boundary_guard() {
        let mut registrar = bmux_plugin_sdk::ContributionRegistrar::new();
        super::PaneRuntimePlugin::register_contributions(&mut registrar).unwrap();
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
        assert_eq!(routes.len(), 27);
        assert!(routes.iter().any(|route| {
            route.interface_id == "pane-runtime-view-size-v1"
                && route.operation == "resize-alternate"
                && route.capability == "bmux.pane_runtime.view.size.v1"
                && route.kind == bmux_plugin_sdk::ServiceKind::Command
        }));
        let pointer_routes: std::collections::BTreeSet<_> = routes
            .iter()
            .filter(|route| route.interface_id == "pane-runtime-pointer-grid-v2")
            .map(|route| route.operation.as_str())
            .collect();
        assert_eq!(
            pointer_routes,
            std::collections::BTreeSet::from(["capabilities", "screen", "wait-present"])
        );
        let services = super::PaneRuntimePlugin::provided_services().unwrap();
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
            if route.capability == "bmux.pane_runtime.spawn.v1" {
                assert_eq!(route.kind, bmux_plugin_sdk::ServiceKind::Query);
                assert_eq!(route.interface_id, "pane-runtime-spawn-v1");
            }
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
    fn source_queries_are_discoverable_without_spawn_commands() {
        let services = super::PaneRuntimePlugin::provided_services().unwrap();
        let source_services: Vec<_> = services
            .iter()
            .filter(|service| service.capability.as_str() == "bmux.pane_runtime.spawn.v1")
            .collect();
        assert_ne!(source_services.len(), 0);
        assert!(source_services.iter().all(|service| {
            service.kind == bmux_plugin_sdk::ServiceKind::Query
                && service.interface_id == "pane-runtime-spawn-v1"
        }));
        let mut registrar = bmux_plugin_sdk::ContributionRegistrar::new();
        super::PaneRuntimePlugin::register_contributions(&mut registrar).unwrap();
        let contributions = registrar.into_contributions();
        let [bmux_plugin_sdk::PluginContribution::Extension { payload, .. }] =
            contributions.as_slice()
        else {
            panic!("expected exact route contribution");
        };
        let routes: Vec<bmux_plugin::stream_admission::StreamAdmittedRoute> =
            bmux_plugin_sdk::decode_service_message(payload).unwrap();
        let operations: std::collections::BTreeSet<_> = routes
            .iter()
            .filter(|route| route.interface_id == "pane-runtime-spawn-v1")
            .map(|route| route.operation.as_str())
            .collect();
        assert_eq!(
            operations,
            std::collections::BTreeSet::from([
                "home-directory",
                "capture-source",
                "observe-directory",
            ])
        );
    }

    #[test]
    fn pointer_grid_exports_generated_command_interface() {
        let services = super::PaneRuntimePlugin::provided_services().unwrap();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        assert!(services.iter().any(|service| service.interface_id
            == "pane-runtime-pointer-grid-v2"
            && service.capability.as_str() == "bmux.pane_runtime.pointer_grid.v1"
            && service.kind == bmux_plugin_sdk::ServiceKind::Command));
        assert!(manifest.services.iter().any(|service| service.interface_id
            == "pane-runtime-pointer-grid-v2"
            && service.kind == bmux_plugin_sdk::ServiceKind::Command));
    }

    #[test]
    fn readiness_commands_are_discoverable() {
        let services = super::PaneRuntimePlugin::provided_services().unwrap();
        assert!(services.iter().any(
            |service| service.interface_id == "pane-runtime-readiness-v1"
                && service.capability.as_str() == "bmux.pane_runtime.write"
                && service.kind == bmux_plugin_sdk::ServiceKind::Command
        ));
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        assert!(
            manifest
                .services
                .iter()
                .any(|service| service.interface_id == "pane-runtime-readiness-v1")
        );
    }

    #[test]
    fn only_cohesive_execution_contract_is_discoverable() {
        let services = super::PaneRuntimePlugin::provided_services().unwrap();
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

bmux_plugin_sdk::plugin!(PaneRuntimePlugin);

#[derive(Default)]
pub struct PaneRuntimePlugin {
    /// The host handles this instance's activation received.
    host: switchy_thread::sync::RwLock<runtime::PaneRuntimeHost>,
}

fn slot_mut<T>(
    slot: &switchy_thread::sync::RwLock<T>,
) -> std::result::Result<
    switchy_thread::sync::RwLockWriteGuard<'_, T>,
    bmux_plugin_sdk::PluginCommandError,
> {
    slot.write()
        .map_err(|_| bmux_plugin_sdk::PluginCommandError::failed("plugin state lock poisoned"))
}

impl PaneRuntimePlugin {
    fn host(&self) -> runtime::PaneRuntimeHost {
        self.host
            .read()
            .map(|host| host.clone())
            .unwrap_or_default()
    }

    /// Services the generated manifest declares. The BPDL world provides the
    /// read-only spawn queries but not the unsupported spawn commands.
    fn provided_services() -> bmux_plugin_sdk::Result<Vec<bmux_plugin_sdk::PluginService>> {
        plugin_definition()
            .declarations()
            .map(|declarations| declarations.services)
            .map_err(
                |error| bmux_plugin_sdk::PluginError::InvalidNativeServiceOutput {
                    plugin_id: declaration::PLUGIN_ID.into(),
                    details: error.to_string(),
                },
            )
    }
}

impl Plugin for PaneRuntimePlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(Self::activate)
            .blocking_deactivate(Self::deactivate)
            .blocking_command("pane-padding-show", Self::run_command)
            .blocking_command("pane-padding-set", Self::run_command)
            .blocking_command("pane-padding-configure", Self::run_command)
            .blocking_command("pane-padding-reset", Self::run_command)
            .blocking_services(Self::invoke_service, None)
            .typed_services(|_, context, registry| {
                Self::register_typed_services(&context, registry);
                Ok(())
            })
            .registrations(|_, a0| Self::register_contributions(a0));
    }
}

impl PaneRuntimePlugin {
    fn register_contributions(
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
                "bmux.pane_runtime.view.size.v1",
                ServiceKind::Command,
                "pane-runtime-view-size-v1",
                &["resize-alternate"][..],
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
                "pane-runtime-pointer-grid-v2",
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
        let services = Self::provided_services()?;
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

    /// Activate this instance's runtime (synchronous body).
    ///
    /// # Errors
    /// Returns the failure the synchronous body reports.
    pub fn activate(
        &self,
        context: &NativeLifecycleContext,
        _tasks: Option<bmux_plugin_sdk::HostAsyncHandle>,
    ) -> std::result::Result<(), PluginCommandError> {
        // The host handles this activation received; every runtime, handler
        // and task reaches the host through them.
        let states = bmux_plugin::activation_state_registry(context);
        let events = bmux_plugin::activation_event_bus(context);
        *slot_mut(&self.host)? = runtime::PaneRuntimeHost::new(states, events);
        let host = &self.host();
        runtime::activate_managed_scope(host);
        application_resurrection::activate(host, context);
        readiness::activate(host, context);
        let execution_settings =
            runtime::session_control::ExecutionSettings::parse(context.settings.as_ref())
                .map_err(PluginCommandError::invalid_arguments)?;
        runtime::session_control::activate(host, context, execution_settings);
        runtime::reservations::activate(host, context);
        // Register the focus-state channel so subscribers (e.g.
        // the decoration plugin) can observe the focused pane per
        // session. Seeded empty — `publish_focus_state_snapshot`
        // republishes the full map on every session-mutating pane
        // command (via `handlers::publish_focus_state_snapshot`).
        if let Some(events) = host.events() {
            events.register_state_channel::<SessionFocusStateMap>(
                pane_runtime_focus::STATE_KIND,
                SessionFocusStateMap {
                    entries: BTreeMap::new(),
                    revision: 0,
                },
            );
            events.register_channel::<PaneEvent>(pane_runtime_events::EVENT_KIND);
            events.register_channel::<
                bmux_pane_runtime_plugin_api::pane_runtime_terminal_events::TerminalEvent,
            >(bmux_pane_runtime_plugin_api::pane_runtime_terminal_events::EVENT_KIND);
        }
        // Publish the initial snapshot so any already-registered
        // subscribers see the current focus state immediately (even
        // though the map is usually empty at activate time — sessions
        // are created later).
        handlers::publish_focus_state_snapshot(host);
        handlers::pane_commands::publish_pane_lifecycle_facts(host);

        let config = host
            .states()
            .get::<bmux_pane_runtime_plugin_api::PaneRuntimePluginConfig>()
            .and_then(|handle| handle.read().ok().map(|guard| (*guard).clone()))
            .unwrap_or_else(|| bmux_pane_runtime_plugin_api::PaneRuntimePluginConfig {
                shell: switchy_env::var("SHELL")
                    .ok()
                    .filter(|shell| !shell.trim().is_empty())
                    .unwrap_or_else(|| crate::platform::baseline_shell().to_string()),
                pane_term: "xterm-256color".to_string(),
                bracketed_paste: true,
                shell_integration_root: None,
            });
        let padding_config = padding::PanePaddingConfig::parse(context.settings.as_ref())
            .map_err(PluginCommandError::invalid_arguments)?;
        runtime::activate_pane_runtime(host, config, padding_config)
            .map_err(PluginCommandError::failed)?;
        Ok(())
    }

    fn deactivate(
        &self,
        _context: &NativeLifecycleContext,
    ) -> std::result::Result<(), PluginCommandError> {
        let host = &self.host();
        if let Some(handle) = host
            .states()
            .get::<runtime::PanePaddingRuntimeHandle>()
            .and_then(|handle| handle.read().ok().map(|guard| guard.clone()))
        {
            handle
                .cancel_all_previews()
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        }
        if let Ok(service) = host.fact_service().read() {
            service.registry().remove_producer("bmux.pane_runtime");
        }
        Ok(())
    }

    fn run_command(
        &self,
        context: &NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        commands::run(&self.host(), context)?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    /// Serve one hand-routed service invocation (synchronous body).
    pub fn invoke_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        handlers::route(&self.host(), context)
    }

    fn register_typed_services(
        _context: &TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
    }
}

#[cfg(test)]
mod definition_contract_tests {
    #[test]
    fn permissions_policy_is_declared_but_not_required_for_local_admission() {
        let definition = super::plugin_definition();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        assert!(
            manifest
                .optional_capabilities
                .iter()
                .any(|capability| capability.as_str() == "bmux.sessions.policy")
        );
        assert!(
            !manifest
                .required_capabilities
                .iter()
                .any(|capability| capability.as_str() == "bmux.sessions.policy")
        );
        assert!(
            !definition
                .metadata
                .required_capabilities
                .iter()
                .any(|capability| capability == "bmux.sessions.policy")
        );
    }

    #[test]
    fn explicit_contract_versions_preserve_service_wire_routes() {
        let definition = super::plugin_definition();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        definition.validate().unwrap();
        assert_eq!(definition.metadata.plugin_id, manifest.id);
        assert_eq!(definition.metadata.version, manifest.version);
        assert_eq!(definition.metadata.roles, manifest.component_roles);
        let declared: std::collections::BTreeSet<_> = manifest
            .contracts
            .iter()
            .map(|contract| (&contract.interface_id, contract.major, contract.minor))
            .collect();
        let embedded: std::collections::BTreeSet<_> = definition
            .metadata
            .contracts
            .iter()
            .map(|contract| (&contract.interface_id, contract.major, contract.minor))
            .collect();
        assert_eq!(embedded, declared);
        let mut services = definition.declarations().unwrap().services;
        services.extend(super::PaneRuntimePlugin::provided_services().unwrap());
        for service in services {
            let contract = definition
                .metadata
                .contracts
                .iter()
                .find(|contract| contract.interface_id == service.interface_id)
                .expect("every retained route requires explicit compatibility metadata");
            // A versioned route (`name/vN`) negotiates major N; the rest are v1.
            let major = service
                .interface_id
                .rsplit_once("/v")
                .map_or(1, |(_, version)| version.parse::<u16>().unwrap());
            assert_eq!(
                (contract.major, contract.minor),
                (major, 0),
                "{}",
                service.interface_id
            );
        }
    }
}
