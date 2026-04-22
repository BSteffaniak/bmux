//! bmux pane-runtime plugin.
//!
//! Owns pane runtime for bmux: PTY handles (spawned via
//! `portable-pty`), per-session layout tree, per-pane output fanout
//! buffer, per-pane terminal-protocol + vt100 cursor +
//! shell-integration parsers, per-pane resurrection state, per-session
//! attach viewport.
//!
//! # Current state
//!
//! `activate` registers noop trait-object handles (from
//! `bmux_pane_runtime_state`) into the plugin state registry so
//! server handle lookups always succeed. Typed service handlers
//! dispatch the pane-runtime commands + attach-runtime commands
//! declared by `bpdl/pane-runtime-plugin.bpdl`; handlers that have
//! not been wired yet return a `not_yet_wired` error so callers
//! fall through cleanly.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use std::sync::{Arc, RwLock};

use bmux_pane_runtime_state::PaneOutputReaderHandle;
use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};

#[derive(Default)]
pub struct PaneRuntimePlugin;

impl RustPlugin for PaneRuntimePlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        // Register a no-op `PaneOutputReaderHandle` so server handle
        // lookups always succeed. A real `OutputFanoutBuffer`-backed
        // impl replaces this when the per-connection push path is
        // wired through the handle.
        let handle = Arc::new(RwLock::new(PaneOutputReaderHandle::noop()));
        global_plugin_state_registry().register::<PaneOutputReaderHandle>(&handle);

        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        // Typed service handlers are not yet wired. Callers get a
        // `not_yet_wired` error they can handle as a graceful
        // fallback until the handler routing lands.
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
        // No typed Arc<dyn Trait> surface today — pane-runtime
        // operations dispatch exclusively through the byte-service
        // path.
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

bmux_plugin_sdk::export_plugin!(PaneRuntimePlugin, include_str!("../plugin.toml"));
