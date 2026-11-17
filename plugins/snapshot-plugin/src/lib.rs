//! bmux snapshot orchestration plugin.
//!
//! Walks the `bmux_snapshot_runtime::StatefulPluginRegistry`, builds
//! a combined envelope over every registered `StatefulPlugin`
//! participant, debounces dirty marks, and persists the result to a
//! CLI-configured file path.
//!
//! Consumed through generated `snapshot-state` and `snapshot-commands`
//! services. The plugin registers a `SnapshotOrchestratorHandle` trait
//! object in the plugin state registry so server startup/shutdown code
//! can still drive baseline restore and debounce flushing.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_snapshot_plugin_api::export_schema,
            crate_path: ::bmux_snapshot_plugin_api,
        },
        settings: "plugin.toml",
    }
}

mod managed_catalog;
mod orchestrator;
mod participants;
mod portable;
pub use portable::portable_participant;

pub use bmux_snapshot_plugin_api::envelope::{CombinedSnapshotEnvelope, SectionV1};
pub use orchestrator::BmuxSnapshotOrchestrator;

use std::sync::{Arc, RwLock};

use bmux_plugin_sdk::author::{Declarations, Plugin};
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

/// Explicit host state port: the registry belongs to one host runtime.
#[derive(Clone)]
pub struct SnapshotRuntime {
    pub state: Arc<bmux_plugin::PluginStateRegistry>,
    pub progress: bmux_plugin_runtime::shutdown_progress::ShutdownProgress,
    /// Exact managed target and public identity selected by the host.
    /// None means explicitly non-managed, never environment fallback.
    pub managed_target: Option<(String, String)>,
    /// Host-owned startup admission. False during ordinary live operation.
    /// The host takes the write lock to close admission and drain restores.
    pub participant_restore_admitted: Arc<RwLock<bool>>,
    /// Open only for the owning host's final-snapshot shutdown phase.
    pub participant_quiesce_admitted: Arc<RwLock<bool>>,
}

impl SnapshotRuntime {
    /// Bind production capture and storage to the registry selected by the host.
    ///
    /// This must be the same registry receiving `SnapshotPluginConfig`, live
    /// `StatefulPluginRegistry` participants and `SnapshotDirtyFlagHandle`.
    /// No registry, path, participant projection or managed identity is inferred
    /// from process environment. The supplied managed pair is (target, identity).
    /// Keep a clone to control restore/quiesce admission at host lifecycle edges.
    ///
    /// # Errors
    /// Rejects an empty managed identity or poisoned shared progress state.
    pub fn from_host_registry(
        state: Arc<bmux_plugin::PluginStateRegistry>,
        managed_target: Option<(String, String)>,
    ) -> std::result::Result<Self, PluginCommandError> {
        if managed_target
            .as_ref()
            .is_some_and(|(target, identity)| target.is_empty() || identity.is_empty())
        {
            return Err(PluginCommandError::failed(
                "snapshot managed target and identity must be nonempty",
            ));
        }
        let progress = state
            .get_or_insert_with(bmux_plugin_runtime::shutdown_progress::ShutdownProgress::default)
            .read()
            .map_err(|_| PluginCommandError::failed("snapshot shutdown progress lock poisoned"))?
            .clone();
        Ok(Self {
            progress,
            managed_target,
            ..Self::new(state)
        })
    }

    /// Construct isolated ports; production hosts should use `from_host_registry`
    /// so shutdown progress is shared with the selected lifecycle authority.
    #[must_use]
    pub fn new(state: Arc<bmux_plugin::PluginStateRegistry>) -> Self {
        Self {
            state,
            progress: bmux_plugin_runtime::shutdown_progress::ShutdownProgress::default(),
            managed_target: None,
            participant_restore_admitted: Arc::new(RwLock::new(false)),
            participant_quiesce_admitted: Arc::new(RwLock::new(false)),
        }
    }
}

pub struct SnapshotPlugin {
    runtime: SnapshotRuntime,
    orchestrator: std::sync::RwLock<Option<Arc<BmuxSnapshotOrchestrator>>>,
    bound: bool,
}

fn slot_mut<T>(
    slot: &std::sync::RwLock<T>,
) -> std::result::Result<std::sync::RwLockWriteGuard<'_, T>, bmux_plugin_sdk::PluginCommandError> {
    slot.write()
        .map_err(|_| bmux_plugin_sdk::PluginCommandError::failed("plugin state lock poisoned"))
}

impl SnapshotPlugin {
    #[must_use]
    pub fn new(runtime: SnapshotRuntime) -> Self {
        Self {
            runtime,
            orchestrator: std::sync::RwLock::new(None),
            bound: true,
        }
    }

    fn invoke_participant(&self, context: &NativeServiceContext) -> ServiceResponse {
        let port = || {
            self.runtime
                .state
                .get::<StatefulPluginRegistry>()
                .map(participants::Participants)
                .ok_or_else(|| participants::unavailable("participant registry not registered"))
        };
        bmux_plugin_sdk::route_service!(context, {
            bmux_snapshot_plugin_api::snapshot_participant_state_v1::client::ParticipantsEndpoint => |_req, _ctx| {
                Ok(port().and_then(|port| port.ids()))
            },
            bmux_snapshot_plugin_api::snapshot_participant_state_v1::client::RestoreDependenciesEndpoint => |req, _ctx| {
                Ok(port().and_then(|port| port.dependencies(&req.participant_id)))
            },
            bmux_snapshot_plugin_api::snapshot_participant_state_v1::client::SnapshotEndpoint => |req, _ctx| {
                Ok(port().and_then(|port| port.snapshot(&req.participant_id)))
            },
            bmux_snapshot_plugin_api::snapshot_participant_state_v1::client::ValidateEndpoint => |req, _ctx| {
                Ok(port().and_then(|port| port.validate(req.snapshot)))
            },
            bmux_snapshot_plugin_api::snapshot_participant_commands_v1::client::QuiesceEndpoint => |req, _ctx| {
                let quiesce = || {
                    let admission = self.runtime.participant_quiesce_admitted.read()
                        .map_err(|_| participants::unavailable("participant quiesce admission poisoned"))?;
                    if !*admission {
                        return Err(participants::unavailable("participant quiesce requires host shutdown admission"));
                    }
                    let result = port().and_then(|port| port.quiesce(&req.participant_id));
                    drop(admission);
                    result
                };
                Ok(quiesce())
            },
            bmux_snapshot_plugin_api::snapshot_participant_commands_v1::client::RestoreEndpoint => |req, _ctx| {
                let restore = || {
                    let admission = self.runtime.participant_restore_admitted.read()
                        .map_err(|_| participants::unavailable("participant restore admission poisoned"))?;
                    if !*admission {
                        return Err(participants::unavailable("participant restore requires host startup admission"));
                    }
                    // Keep the read guard through validation/application. Closing
                    // startup admission takes the write lock and drains restores.
                    let result = port().and_then(|port| port.restore(req.snapshot));
                    drop(admission);
                    result
                };
                Ok(restore())
            },
        })
    }

    fn orchestrator(&self) -> Option<Arc<BmuxSnapshotOrchestrator>> {
        self.orchestrator
            .read()
            .ok()
            .and_then(|orchestrator| orchestrator.clone())
    }

    fn snapshot_handle(&self) -> Result<SnapshotOrchestratorHandle, snapshot_types::SnapshotError> {
        snapshot_handle(&self.runtime.state)
    }
}

impl Default for SnapshotPlugin {
    fn default() -> Self {
        Self {
            runtime: SnapshotRuntime::new(Arc::new(bmux_plugin::PluginStateRegistry::new())),
            orchestrator: std::sync::RwLock::new(None),
            bound: false,
        }
    }
}

impl Plugin for SnapshotPlugin {
    fn declare(d: &mut Declarations<Self>) {
        d.blocking_activate(Self::activate)
            .blocking_deactivate(Self::deactivate)
            .blocking_services(Self::invoke_service, None)
            .typed_services(|plugin, context, registry| {
                plugin.register_typed_services(context, registry);
                Ok(())
            });
    }
}

impl SnapshotPlugin {
    fn activate(
        &self,
        _context: NativeLifecycleContext,
        _tasks: Option<bmux_plugin_sdk::HostAsyncHandle>,
    ) -> std::result::Result<(), PluginCommandError> {
        if !self.bound || self.orchestrator().is_some() {
            return Err(PluginCommandError::unavailable(
                "snapshot runtime unbound or already activated",
            ));
        }
        let registry = &self.runtime.state;
        if registry.get::<SnapshotOrchestratorHandle>().is_some() {
            return Err(PluginCommandError::unavailable(
                "snapshot authority already registered in this host",
            ));
        }

        // Pull the CLI-registered config if present; otherwise activate
        // in disabled mode (orchestrator ops return Ok(None) / Disabled).
        let config: Option<SnapshotPluginConfig> = registry
            .get::<SnapshotPluginConfig>()
            .map(|handle| {
                handle
                    .read()
                    .map(|g| (*g).clone())
                    .map_err(|_| PluginCommandError::failed("snapshot configuration lock poisoned"))
            })
            .transpose()?;

        // Ensure a shared dirty flag exists and is registered so every
        // server mutation site can flip it without needing to look the
        // orchestrator up on the hot path.
        let dirty_handle_entry = bmux_snapshot_runtime::get_or_init_stateful_dirty_flag(
            || registry.get(),
            |fresh| {
                registry.register_communal::<SnapshotDirtyFlagHandle>(fresh);
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
                registry.register_communal::<StatefulPluginRegistry>(fresh);
            },
        );

        let managed_catalog = match (&config, &self.runtime.managed_target) {
            (Some(config), Some((target, identity))) => Some(
                managed_catalog::ManagedCatalog::new(
                    &config.snapshot_path,
                    target.clone(),
                    identity,
                )
                .map_err(|error| PluginCommandError::failed(error.to_string()))?,
            ),
            _ => None,
        };
        if let Some(catalog) = &managed_catalog {
            let discovery = catalog
                .discovery()
                .map_err(|error| PluginCommandError::failed(error.to_string()))?;
            tracing::warn!(%discovery, "managed snapshot layout ownership and discovery");
        }
        // Build the concrete orchestrator.
        let orchestrator = BmuxSnapshotOrchestrator::new(
            config.as_ref().map(|c| c.snapshot_path.clone()),
            Arc::clone(&dirty_flag_arc),
            Arc::clone(&stateful_registry),
        )
        .with_managed_catalog(managed_catalog)
        .with_progress(self.runtime.progress.clone());
        let orchestrator = Arc::new(orchestrator);
        *slot_mut(&self.orchestrator)? = Some(Arc::clone(&orchestrator));
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

        let lifecycle =
            registry.get_or_insert_with(bmux_plugin_runtime::shutdown::ShutdownRegistry::default);
        lifecycle
            .read()
            .map_err(|_| PluginCommandError::failed("shutdown registry poisoned"))?
            .register(Arc::new(SnapshotShutdown(Arc::clone(&orchestrator))))
            .map_err(PluginCommandError::failed)?;

        Ok(())
    }

    fn deactivate(
        &self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<(), PluginCommandError> {
        if let Some(orchestrator) = &self.orchestrator() {
            orchestrator
                .stop_debounce()
                .map_err(PluginCommandError::failed)?;
        }
        // Deactivation does not discard retained snapshot/recovery authority.
        Ok(())
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        if matches!(
            context.request.service.interface_id.as_str(),
            "snapshot-participant-state-v1" | "snapshot-participant-commands-v1"
        ) {
            return self.invoke_participant(&context);
        }
        bmux_plugin_sdk::route_service!(context, {
            bmux_snapshot_plugin_api::snapshot_state::client::StatusEndpoint => |_req, _ctx| {
                Ok(self.snapshot_handle().map(|handle| handle_status(&handle)))
            },
            bmux_snapshot_plugin_api::snapshot_state::client::RestoreDryRunEndpoint => |_req, _ctx| {
                Ok(self.snapshot_handle().and_then(handle_restore_dry_run))
            },
            bmux_snapshot_plugin_api::snapshot_commands::client::SaveNowEndpoint => |_req, _ctx| {
                Ok(self.snapshot_handle().and_then(handle_save_now))
            },
            bmux_snapshot_plugin_api::snapshot_commands::client::RestoreApplyEndpoint => |_req, _ctx| {
                Ok(self.snapshot_handle().and_then(handle_restore_apply))
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

fn snapshot_handle(
    registry: &bmux_plugin::PluginStateRegistry,
) -> Result<SnapshotOrchestratorHandle, snapshot_types::SnapshotError> {
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

fn handle_save_now(
    orchestrator_handle: SnapshotOrchestratorHandle,
) -> Result<snapshot_types::SnapshotSaveResult, snapshot_types::SnapshotError> {
    let handle = tokio_handle()?;
    futures_block_on(&handle, async move {
        orchestrator_handle.as_dyn().save_now_boxed().await
    })
    .map(|path| snapshot_types::SnapshotSaveResult {
        path: path.map(|p| p.display().to_string()),
    })
    .map_err(|err| error_response("save_failed", &err))
}

fn handle_status(
    orchestrator_handle: &SnapshotOrchestratorHandle,
) -> snapshot_types::SnapshotStatusPayload {
    let report = orchestrator_handle.as_dyn().status();
    snapshot_types::SnapshotStatusPayload {
        enabled: report.enabled,
        path: report.path,
        snapshot_exists: report.snapshot_exists,
        last_write_epoch_ms: report.last_write_epoch_ms,
        last_restore_epoch_ms: report.last_restore_epoch_ms,
        last_restore_error: report.last_restore_error,
    }
}

fn handle_restore_dry_run(
    orchestrator_handle: SnapshotOrchestratorHandle,
) -> Result<snapshot_types::SnapshotDryRunResult, snapshot_types::SnapshotError> {
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
fn handle_restore_apply(
    orchestrator_handle: SnapshotOrchestratorHandle,
) -> Result<snapshot_types::SnapshotApplySummary, snapshot_types::SnapshotError> {
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

struct SnapshotShutdown(Arc<BmuxSnapshotOrchestrator>);
impl bmux_plugin_runtime::shutdown::ShutdownParticipant for SnapshotShutdown {
    fn id(&self) -> &'static str {
        "bmux.snapshot"
    }
    fn phase(&self) -> bmux_plugin_runtime::shutdown::ShutdownPhase {
        bmux_plugin_runtime::shutdown::ShutdownPhase::Persist
    }
    fn shutdown(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'static>>
    {
        let orchestrator = Arc::clone(&self.0);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                orchestrator.stop_debounce()?;
                bmux_snapshot_runtime::SnapshotOrchestrator::finalize(orchestrator.as_ref())
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())?
        })
    }
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
    let (stop, stopped) = std::sync::mpsc::channel();
    let owner = Arc::clone(&orchestrator);
    let thread = std::thread::Builder::new()
        .name("bmux-snapshot-debounce".into())
        .spawn(move || {
            debug!("snapshot debounce loop started (debounce_ms={debounce_ms})");
            loop {
                match stopped.recv_timeout(std::time::Duration::from_millis(DEBOUNCE_POLL_MS)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
                if Arc::strong_count(&orchestrator) <= 1 {
                    // The only remaining Arc is ours; orchestrator is
                    // going away, exit the loop.
                    break;
                }
                if !orchestrator.writes_allowed().unwrap_or(false) {
                    continue;
                }
                if dirty_flag.take_dirty_after_debounce(debounce_ms).is_some()
                    && let Err(err) = orchestrator.save_now_blocking()
                {
                    dirty_flag.mark_dirty();
                    warn!("snapshot debounce flush failed: {err}");
                }
            }
            debug!("snapshot debounce loop exited");
        })
        .expect("failed to spawn snapshot debounce thread");
    owner.install_debounce(stop, thread);
}

/// Build one host-scoped definition. All instances created from it use the
/// explicitly selected host ports, while retaining separate implementation state.
///
/// # Panics
/// Panics if the embedded manifest is rejected, including invalid contract declarations.
#[must_use]
pub fn plugin_definition_with_runtime(
    runtime: SnapshotRuntime,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    bmux_plugin_sdk::author::definition::<SnapshotPlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        crate::declaration::SERVICE_CONTRACTS,
        move |_| Ok(SnapshotPlugin::new(runtime.clone())),
    )
    .expect("embedded snapshot declaration must be valid")
}

// Unbound discovery remains inert. Creation fails rather than silently using an
// empty registry and accepting an apparently successful, incomplete snapshot.
bmux_plugin_sdk::plugin!(
    SnapshotPlugin,
    create = |_| Err(PluginCommandError::unavailable(
        "snapshot requires explicit host runtime ports",
    ))
);

#[cfg(test)]
mod scoped_runtime_tests {
    use super::*;

    #[test]
    fn production_ports_retain_selected_authority_and_target() {
        let state = Arc::new(bmux_plugin::PluginStateRegistry::new());
        let participants = state.get_or_insert_with(StatefulPluginRegistry::new);
        let config = state.get_or_insert_with(|| SnapshotPluginConfig {
            snapshot_path: std::path::PathBuf::from("selected-host/snapshot.json"),
            debounce_ms: 1000,
        });
        let dirty = state.get_or_insert_with(SnapshotDirtyFlagHandle::new);
        let runtime = SnapshotRuntime::from_host_registry(
            Arc::clone(&state),
            Some(("selected-target".into(), "selected-identity".into())),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&runtime.state, &state));
        assert!(Arc::ptr_eq(
            &runtime.state.get::<StatefulPluginRegistry>().unwrap(),
            &participants
        ));
        assert!(Arc::ptr_eq(
            &runtime.state.get::<SnapshotPluginConfig>().unwrap(),
            &config
        ));
        assert!(Arc::ptr_eq(
            &runtime.state.get::<SnapshotDirtyFlagHandle>().unwrap(),
            &dirty
        ));
        assert_eq!(
            runtime.managed_target,
            Some(("selected-target".into(), "selected-identity".into()))
        );
        assert!(!*runtime.participant_restore_admitted.read().unwrap());
        assert!(!*runtime.participant_quiesce_admitted.read().unwrap());
        assert!(
            SnapshotRuntime::from_host_registry(state, Some((String::new(), "identity".into())),)
                .is_err()
        );
    }

    #[test]
    fn host_ports_are_explicit_and_lifecycle_admission_defaults_closed() {
        let a = SnapshotRuntime::new(Arc::new(bmux_plugin::PluginStateRegistry::new()));
        let b = SnapshotRuntime::new(Arc::new(bmux_plugin::PluginStateRegistry::new()));
        assert!(!Arc::ptr_eq(&a.state, &b.state));
        assert!(!*a.participant_restore_admitted.read().unwrap());
        assert!(!*a.participant_quiesce_admitted.read().unwrap());
        *a.participant_restore_admitted.write().unwrap() = true;
        assert!(!*b.participant_restore_admitted.read().unwrap());
        assert!(!SnapshotPlugin::default().bound);
    }
}

#[cfg(test)]
mod definition_contract_tests {
    #[test]
    fn explicit_contract_versions_preserve_service_wire_routes() {
        let definition = super::plugin_definition();
        let manifest =
            bmux_plugin::PluginManifest::from_toml_str(crate::declaration::PLUGIN_MANIFEST)
                .unwrap();
        let injected = super::plugin_definition_with_runtime(super::SnapshotRuntime::new(
            std::sync::Arc::new(bmux_plugin::PluginStateRegistry::new()),
        ));
        injected.validate().unwrap();
        assert_eq!(injected.metadata, definition.metadata);
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
        let services = definition.declarations().unwrap().services;
        for service in services {
            let contract = definition
                .metadata
                .contracts
                .iter()
                .find(|contract| contract.interface_id == service.interface_id)
                .expect("every retained route requires explicit compatibility metadata");
            assert_eq!((contract.major, contract.minor), (1, 0));
        }
    }
}
