//! bmux pane-runtime plugin.
//!
//! Declares capabilities + typed service interfaces for pane-runtime
//! operations. The authoritative `SessionRuntimeManager` + its
//! `SessionRuntimeManagerApi` handle are registered by `bmux_server`
//! during `BmuxServer::new`; this plugin intentionally does not
//! construct a second manager. Plugin role:
//!
//! - Hold the `bmux.pane_runtime` capability/feature declarations so
//!   other plugins may declare typed dependencies on it.
//! - Provide a future home for typed service handlers that translate
//!   BPDL requests into `SessionRuntimeManagerApi` calls.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};

#[derive(Default)]
pub struct PaneRuntimePlugin;

impl RustPlugin for PaneRuntimePlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // No-op: server owns the `SessionRuntimeManager` and registers
        // the `SessionRuntimeManagerHandle` during `BmuxServer::new`.
        // This plugin exists today to declare the capability + feature
        // surface; typed service handlers will land in a follow-up.
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        let _ = context;
        ServiceResponse::error(
            "not_yet_wired",
            "pane-runtime plugin service handlers are not yet wired",
        )
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
    }
}

// Keep the capability/interface constants alive for consumers of the
// exported plugin binary (plugin.toml references them by string; Rust
// doesn't see that wiring).
const _KEEPS_CONSTS_ALIVE: (
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
    bmux_plugin_sdk::CapabilityId,
) = (
    bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_READ,
    bmux_pane_runtime_plugin_api::capabilities::PANE_RUNTIME_WRITE,
    bmux_pane_runtime_plugin_api::capabilities::ATTACH_RUNTIME_READ,
    bmux_pane_runtime_plugin_api::capabilities::ATTACH_RUNTIME_WRITE,
);
