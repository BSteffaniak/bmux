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
//! - Provide typed service handlers that translate BPDL requests into
//!   calls against the registered `SessionRuntimeManagerHandle`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use bmux_pane_runtime_plugin_api::pane_runtime_focus::{self, SessionFocusStateMap};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use std::collections::BTreeMap;

mod handlers;

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
        // Publish the initial snapshot so any already-registered
        // subscribers see the current focus state immediately (even
        // though the map is usually empty at activate time — sessions
        // are created later).
        handlers::publish_focus_state_snapshot();
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
