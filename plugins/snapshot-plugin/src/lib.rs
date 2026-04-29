//! bmux snapshot orchestration plugin.
//!
//! Walks the `bmux_snapshot_runtime::StatefulPluginRegistry`, builds
//! a combined envelope over every registered `StatefulPlugin`
//! participant, debounces dirty marks, and persists the result to a
//! CLI-configured file path.
//!
//! Consumed by the server via the `SnapshotOrchestratorHandle` trait
//! object registered in the plugin state registry. `Request::ServerSave
//! / ServerRestoreDryRun / ServerRestoreApply / ServerStatus` IPC
//! handlers on the server side delegate to the orchestrator through
//! the handle.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

mod orchestrator;

pub use bmux_snapshot_plugin_api::envelope::{CombinedSnapshotEnvelope, SectionV1};
pub use orchestrator::BmuxSnapshotOrchestrator;

use std::sync::{Arc, RwLock};

use bmux_plugin::global_plugin_state_registry;
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_snapshot_plugin_api::{SnapshotPluginConfig, snapshot_types};
use bmux_snapshot_runtime::{
    SnapshotDirtyFlagHandle, SnapshotOrchestratorError, SnapshotOrchestratorHandle,
    StatefulPluginRegistry,
};
use tracing::{debug, warn};

/// Default debounce window when config doesn't supply one.
const DEFAULT_DEBOUNCE_MS: u64 = 1_000;
/// Poll tick for the debounce-flush background thread.
const DEBOUNCE_POLL_MS: u64 = 200;

#[derive(Default)]
pub struct SnapshotPlugin;

impl RustPlugin for SnapshotPlugin {
    fn activate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        let registry = global_plugin_state_registry();

        // Pull the CLI-registered config if present; otherwise activate
        // in disabled mode (orchestrator ops return Ok(None) / Disabled).
        let config: Option<SnapshotPluginConfig> = registry
            .get::<SnapshotPluginConfig>()
            .and_then(|handle| handle.read().ok().map(|g| (*g).clone()));

        // Ensure a shared dirty flag exists and is registered so every
        // server mutation site can flip it without needing to look the
        // orchestrator up on the hot path.
        let dirty_handle_entry = bmux_snapshot_runtime::get_or_init_stateful_dirty_flag(
            || registry.get(),
            |fresh| {
                registry.register::<SnapshotDirtyFlagHandle>(fresh);
            },
        );
        let dirty_flag_arc = {
            let guard = dirty_handle_entry
                .read()
                .expect("snapshot dirty flag handle lock poisoned");
            Arc::clone(&guard.0)
        };

        // Ensure the stateful registry exists (plugins that activated
        // earlier will have populated it; we register a fresh empty
        // one as a no-op fallback in headless tests).
        let stateful_registry = bmux_snapshot_runtime::get_or_init_stateful_registry(
            || registry.get::<StatefulPluginRegistry>(),
            |fresh| {
                registry.register::<StatefulPluginRegistry>(fresh);
            },
        );

        // Build the concrete orchestrator.
        let orchestrator = BmuxSnapshotOrchestrator::new(
            config.as_ref().map(|c| c.snapshot_path.clone()),
            Arc::clone(&dirty_flag_arc),
            Arc::clone(&stateful_registry),
        );
        let orchestrator = Arc::new(orchestrator);
        let handle = SnapshotOrchestratorHandle::from_shared(Arc::clone(&orchestrator));
        let handle_entry = Arc::new(RwLock::new(handle));
        registry.register::<SnapshotOrchestratorHandle>(&handle_entry);

        // Spawn debounce-flush loop only when a path is configured.
        if let Some(config) = config {
            spawn_debounce_loop(
                Arc::clone(&orchestrator),
                Arc::clone(&dirty_flag_arc),
                if config.debounce_ms == 0 {
                    DEFAULT_DEBOUNCE_MS
                } else {
                    config.debounce_ms
                },
            );
        }

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
            "snapshot-state", "status" => |_req: (), _ctx| {
                Ok::<Result<snapshot_types::SnapshotStatusPayload, snapshot_types::SnapshotError>, ServiceResponse>(handle_status())
            },
            "snapshot-state", "restore-dry-run" => |_req: (), _ctx| {
                Ok::<Result<snapshot_types::SnapshotDryRunResult, snapshot_types::SnapshotError>, ServiceResponse>(handle_restore_dry_run())
            },
            "snapshot-commands", "save-now" => |_req: (), _ctx| {
                Ok::<Result<snapshot_types::SnapshotSaveResult, snapshot_types::SnapshotError>, ServiceResponse>(handle_save_now())
            },
            "snapshot-commands", "restore-apply" => |_req: (), _ctx| {
                Ok::<Result<snapshot_types::SnapshotApplySummary, snapshot_types::SnapshotError>, ServiceResponse>(handle_restore_apply())
            },
        })
    }

    fn register_typed_services(
        &self,
        _context: TypedServiceRegistrationContext<'_>,
        _registry: &mut TypedServiceRegistry,
    ) {
        // No typed Arc<dyn Trait> surface today — generated snapshot
        // operations dispatch through the byte-service path.
    }
}

fn snapshot_handle() -> Result<SnapshotOrchestratorHandle, snapshot_types::SnapshotError> {
    let registry = global_plugin_state_registry();
    let Some(handle_entry) = registry.get::<SnapshotOrchestratorHandle>() else {
        return Err(snapshot_types::SnapshotError::NotRegistered {
            message: "snapshot orchestrator not registered".into(),
        });
    };
    handle_entry.read().map(|guard| guard.clone()).map_err(|_| {
        snapshot_types::SnapshotError::LockPoisoned {
            message: "snapshot orchestrator handle lock poisoned".into(),
        }
    })
}

fn tokio_handle() -> Result<tokio::runtime::Handle, snapshot_types::SnapshotError> {
    tokio::runtime::Handle::try_current().map_err(|_| snapshot_types::SnapshotError::NoRuntime {
        message: "tokio runtime not available for snapshot dispatch".into(),
    })
}

fn handle_save_now() -> Result<snapshot_types::SnapshotSaveResult, snapshot_types::SnapshotError> {
    let orchestrator_handle = snapshot_handle()?;
    let handle = tokio_handle()?;
    futures_block_on(&handle, async move {
        orchestrator_handle.as_dyn().save_now_boxed().await
    })
    .map(|path| snapshot_types::SnapshotSaveResult {
        path: path.map(|p| p.display().to_string()),
    })
    .map_err(|err| error_response("save_failed", &err))
}

fn handle_status() -> Result<snapshot_types::SnapshotStatusPayload, snapshot_types::SnapshotError> {
    let orchestrator_handle = snapshot_handle()?;
    let report = orchestrator_handle.as_dyn().status();
    Ok(snapshot_types::SnapshotStatusPayload {
        enabled: report.enabled,
        path: report.path,
        snapshot_exists: report.snapshot_exists,
        last_write_epoch_ms: report.last_write_epoch_ms,
        last_restore_epoch_ms: report.last_restore_epoch_ms,
        last_restore_error: report.last_restore_error,
    })
}

fn handle_restore_dry_run()
-> Result<snapshot_types::SnapshotDryRunResult, snapshot_types::SnapshotError> {
    let orchestrator_handle = snapshot_handle()?;
    let handle = tokio_handle()?;
    futures_block_on(&handle, async move {
        orchestrator_handle.as_dyn().dry_run_boxed().await
    })
    .map(|report| snapshot_types::SnapshotDryRunResult {
        ok: report.ok,
        message: report.message,
    })
    .map_err(|err| error_response("dry_run_failed", &err))
}

#[allow(clippy::cast_possible_truncation)]
fn handle_restore_apply()
-> Result<snapshot_types::SnapshotApplySummary, snapshot_types::SnapshotError> {
    let orchestrator_handle = snapshot_handle()?;
    let handle = tokio_handle()?;
    futures_block_on(&handle, async move {
        orchestrator_handle.as_dyn().restore_apply_boxed().await
    })
    .map(|summary| snapshot_types::SnapshotApplySummary {
        restored_plugins: summary.restored_plugins as u64,
        failed_plugins: summary.failed_plugins as u64,
    })
    .map_err(|err| error_response("restore_failed", &err))
}

fn error_response(code: &str, err: &SnapshotOrchestratorError) -> snapshot_types::SnapshotError {
    snapshot_types::SnapshotError::Failed {
        code: code.to_string(),
        message: err.to_string(),
    }
}

/// Drive an async future to completion from a synchronous context
/// when a tokio runtime is available. Bundled plugins share the host
/// tokio runtime, so `block_in_place` is safe here.
fn futures_block_on<F, T>(rt: &tokio::runtime::Handle, fut: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send,
{
    tokio::task::block_in_place(|| rt.block_on(fut))
}

fn spawn_debounce_loop(
    orchestrator: Arc<BmuxSnapshotOrchestrator>,
    dirty_flag: Arc<bmux_snapshot_runtime::SnapshotDirtyFlag>,
    debounce_ms: u64,
) {
    // Use a dedicated OS thread: plugins do not own a tokio runtime
    // of their own, and we want the flush cadence to be independent
    // of any host scheduling decisions. The thread blocks on
    // `thread::sleep` between ticks.
    std::thread::Builder::new()
        .name("bmux-snapshot-debounce".into())
        .spawn(move || {
            debug!("snapshot debounce loop started (debounce_ms={debounce_ms})");
            loop {
                std::thread::sleep(std::time::Duration::from_millis(DEBOUNCE_POLL_MS));
                if Arc::strong_count(&orchestrator) <= 1 {
                    // The only remaining Arc is ours; orchestrator is
                    // going away, exit the loop.
                    break;
                }
                if dirty_flag.take_dirty_after_debounce(debounce_ms).is_some()
                    && let Err(err) = orchestrator.save_now_blocking()
                {
                    warn!("snapshot debounce flush failed: {err}");
                }
            }
            debug!("snapshot debounce loop exited");
        })
        .expect("failed to spawn snapshot debounce thread");
}

bmux_plugin_sdk::export_plugin!(SnapshotPlugin, include_str!("../plugin.toml"));
