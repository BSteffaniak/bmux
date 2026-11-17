//! bmux control-catalog plugin — cross-cutting catalog aggregator.
//!
//! Subscribes to [`SessionEvent`], [`ContextEvent`], and [`ClientEvent`]
//! on the plugin event bus, maintains a monotonic revision counter,
//! and exposes a typed `control-catalog-state::snapshot` query that
//! returns an aggregate view over sessions, contexts, and
//! context-to-session bindings.
//!
//! Attach UIs use this plugin as a single-shot catalog snapshot source
//! (formerly `Request::ControlCatalogSnapshot` in `bmux_ipc`).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

/// Plugin declaration generated from the BPDL world and `plugin.toml`.
pub mod declaration {
    bmux_plugin_schema_macros::plugin_manifest! {
        package: {
            schema: ::bmux_control_catalog_plugin_api::export_schema,
            crate_path: ::bmux_control_catalog_plugin_api,
        },
        settings: "plugin.toml",
    }
}

use bmux_clients_plugin_api::clients_events::{self, ClientEvent};
use bmux_context_state::ContextStateHandle;
use bmux_contexts_plugin_api::contexts_events::{self, ContextEvent};
use bmux_control_catalog_plugin_api::control_catalog_events::{self, CatalogEvent, CatalogScope};
use bmux_control_catalog_plugin_api::control_catalog_state::{
    self, ContextRow, ContextSessionBinding, SessionRow, Snapshot,
};
use bmux_plugin::{EventBus, PluginStateRegistry, ServiceCaller, TypedServiceCaller};
use bmux_plugin_sdk::prelude::*;
use bmux_plugin_sdk::{TypedServiceRegistrationContext, TypedServiceRegistry};
use bmux_session_state::SessionManagerHandle;
use bmux_sessions_plugin_api::sessions_events::{self, SessionEvent};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Revisions are local to the catalog authority, not shared across hosts.
struct CatalogState {
    revision: AtomicU64,
    stop: std::sync::atomic::AtomicBool,
    /// The bus this instance was composed with or its activation received.
    /// `None` until activation supplies one; there is no process-wide bus.
    events: std::sync::RwLock<Option<Arc<EventBus>>>,
    /// Whether the host composed this instance with an explicit bus, which
    /// activations never replace.
    composed: bool,
    disconnected: std::sync::atomic::AtomicBool,
}

impl CatalogState {
    fn events(&self) -> Option<Arc<EventBus>> {
        self.events.read().ok().and_then(|events| events.clone())
    }

    /// Adopt this activation's bus (replacing a previous activation's)
    /// unless the host composed this instance with an explicit one.
    fn adopt_activation_bus(&self, take: impl FnOnce() -> Option<Arc<EventBus>>) {
        if self.composed {
            return;
        }
        if let Ok(mut events) = self.events.write() {
            *events = take();
        }
    }

    fn bump(&self, scopes: Vec<CatalogScope>, full_resync: bool) {
        let revision = self
            .revision
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        let Some(events) = self.events() else {
            return;
        };
        let _ = events.emit(
            &control_catalog_events::EVENT_KIND,
            CatalogEvent::Changed {
                revision,
                scopes,
                full_resync,
            },
        );
    }
}

/// Argument record for the `snapshot` query.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnapshotArgs {
    #[serde(default)]
    since_revision: Option<u64>,
}

pub struct ControlCatalogPlugin {
    catalog: Arc<CatalogState>,
    /// The registry this instance was composed with or its activation
    /// received; an empty private one before activation.
    state: std::sync::RwLock<Arc<PluginStateRegistry>>,
    subscriber: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for ControlCatalogPlugin {
    fn default() -> Self {
        Self::from_parts(None, Arc::new(PluginStateRegistry::new()))
    }
}

impl RustPlugin for ControlCatalogPlugin {
    type Contract = bmux_control_catalog_plugin_api::Contract;

    fn activate(
        &mut self,
        context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        self.catalog
            .adopt_activation_bus(|| bmux_plugin::activation_event_bus(&context));
        self.adopt_activation_states(|| Some(bmux_plugin::activation_state_registry(&context)));
        self.start_subscriber()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn run_command(
        &mut self,
        _context: NativeCommandContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        Err(PluginCommandError::unknown_command(""))
    }

    fn deactivate(
        &mut self,
        _context: NativeLifecycleContext,
    ) -> std::result::Result<i32, PluginCommandError> {
        self.stop_subscriber()?;
        Ok(bmux_plugin_sdk::EXIT_OK)
    }

    fn invoke_service(&self, context: NativeServiceContext) -> ServiceResponse {
        self.service(&context)
    }

    fn register_typed_services(
        &self,
        context: TypedServiceRegistrationContext<'_>,
        registry: &mut TypedServiceRegistry,
    ) {
        let caller = Arc::new(TypedServiceCaller::from_registration_context(&context));

        let state: Arc<dyn control_catalog_state::ControlCatalogStateService + Send + Sync> =
            Arc::new(ControlCatalogStateHandle::new(caller));
        let _ = control_catalog_state::register_provider(registry, state);
    }
}

impl ControlCatalogPlugin {
    /// Bind explicit host mechanisms. Shared/native adapters must use this
    /// constructor rather than the legacy process-global default.
    pub fn with_host(events: Arc<EventBus>, state: Arc<PluginStateRegistry>) -> Self {
        Self::from_parts(Some(events), state)
    }

    fn from_parts(events: Option<Arc<EventBus>>, state: Arc<PluginStateRegistry>) -> Self {
        Self {
            catalog: Arc::new(CatalogState {
                revision: AtomicU64::new(1),
                stop: std::sync::atomic::AtomicBool::new(false),
                disconnected: std::sync::atomic::AtomicBool::new(false),
                composed: events.is_some(),
                events: std::sync::RwLock::new(events),
            }),
            state: std::sync::RwLock::new(state),
            subscriber: std::sync::Mutex::new(None),
        }
    }

    fn states(&self) -> Arc<PluginStateRegistry> {
        self.state.read().map_or_else(
            |_| Arc::new(PluginStateRegistry::new()),
            |state| Arc::clone(&state),
        )
    }

    /// Adopt this activation's registry view unless the host composed this
    /// instance with an explicit one.
    fn adopt_activation_states(&self, take: impl FnOnce() -> Option<Arc<PluginStateRegistry>>) {
        if self.catalog.composed {
            return;
        }
        if let Some(states) = take()
            && let Ok(mut state) = self.state.write()
        {
            *state = states;
        }
    }

    fn service(&self, context: &NativeServiceContext) -> ServiceResponse {
        // The endpoint comes from the BPDL world's provided interfaces, so a
        // route the world does not declare fails to compile.
        use crate::declaration::provides::control_catalog_state::client as state;
        bmux_plugin_sdk::route_service!(context, {
            state::SnapshotEndpoint => |req, _ctx| {
                self.build_snapshot(req.since_revision)
                    .map_err(|error| ServiceResponse::error("snapshot_failed", error))
            },
        })
    }

    fn start_subscriber(&self) -> std::result::Result<(), PluginCommandError> {
        let mut worker = self
            .subscriber
            .lock()
            .map_err(|_| PluginCommandError::failed("catalog subscriber lock poisoned"))?;
        if worker.is_some() {
            return Err(PluginCommandError::failed("catalog already active"));
        }
        let Some(events) = self.catalog.events() else {
            // No host bus: snapshots still serve, revisions do not advance.
            return Ok(());
        };
        events.register_channel::<CatalogEvent>(control_catalog_events::EVENT_KIND);
        let mut sessions = events
            .subscribe::<SessionEvent>(&sessions_events::EVENT_KIND)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let mut contexts = events
            .subscribe::<ContextEvent>(&contexts_events::EVENT_KIND)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        let mut clients = events
            .subscribe::<ClientEvent>(&clients_events::EVENT_KIND)
            .map_err(|error| PluginCommandError::failed(error.to_string()))?;
        self.catalog.stop.store(false, Ordering::Release);
        self.catalog.disconnected.store(false, Ordering::Release);
        let catalog = Arc::clone(&self.catalog);
        *worker = Some(
            std::thread::Builder::new()
                .name("control-catalog".into())
                .spawn(move || {
                    while !catalog.stop.load(Ordering::Acquire) {
                        // Bounded work per source per turn. Lag requests a complete
                        // resync rather than silently terminating the subscriber.
                        for (received, scopes) in [
                            (
                                sessions.try_recv().map(|_| ()),
                                vec![CatalogScope::Sessions, CatalogScope::Bindings],
                            ),
                            (
                                contexts.try_recv().map(|_| ()),
                                vec![CatalogScope::Contexts, CatalogScope::Bindings],
                            ),
                            (clients.try_recv().map(|_| ()), vec![CatalogScope::Sessions]),
                        ] {
                            match received {
                                Ok(()) => catalog.bump(scopes, false),
                                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                                    catalog.bump(scopes, true);
                                }
                                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                                    catalog.bump(scopes, true);
                                    catalog.disconnected.store(true, Ordering::Release);
                                    catalog.stop.store(true, Ordering::Release);
                                }
                                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {}
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                })
                .map_err(|error| PluginCommandError::failed(error.to_string()))?,
        );
        drop(worker);
        Ok(())
    }

    fn stop_subscriber(&self) -> std::result::Result<(), PluginCommandError> {
        self.catalog.stop.store(true, Ordering::Release);
        let worker = self
            .subscriber
            .lock()
            .map_err(|_| PluginCommandError::failed("catalog subscriber lock poisoned"))?
            .take();
        if let Some(worker) = worker {
            worker
                .join()
                .map_err(|_| PluginCommandError::failed("catalog subscriber panicked"))?;
        }
        Ok(())
    }

    fn build_snapshot(&self, _since_revision: Option<u64>) -> Result<Snapshot, String> {
        if self.catalog.disconnected.load(Ordering::Acquire) {
            return Err("catalog event source disconnected; reactivation required".into());
        }
        let revision = self.catalog.revision.load(Ordering::SeqCst);
        let states = self.states();
        let sessions = read_sessions(&states)?;
        let contexts = read_contexts(&states)?;
        let context_session_bindings = read_bindings(&states)?;
        Ok(Snapshot {
            revision,
            sessions,
            contexts,
            context_session_bindings,
        })
    }
}

impl Drop for ControlCatalogPlugin {
    fn drop(&mut self) {
        self.catalog.stop.store(true, Ordering::Release);
        if let Ok(worker) = self.subscriber.get_mut()
            && let Some(worker) = worker.take()
            && worker.join().is_err()
        {
            tracing::warn!("catalog subscriber panicked during destruction");
        }
    }
}

fn read_sessions(state: &PluginStateRegistry) -> Result<Vec<SessionRow>, String> {
    let Some(handle) = state.get::<SessionManagerHandle>() else {
        return Ok(Vec::new());
    };
    let guard = handle
        .read()
        .map_err(|_| "session manager handle lock poisoned".to_string())?;
    Ok(guard
        .0
        .list_sessions()
        .into_iter()
        .map(|info| SessionRow {
            id: info.id.0,
            name: info.name,
            client_count: u32::try_from(info.client_count).unwrap_or(u32::MAX),
        })
        .collect())
}

fn read_contexts(state: &PluginStateRegistry) -> Result<Vec<ContextRow>, String> {
    let Some(handle) = state.get::<ContextStateHandle>() else {
        return Ok(Vec::new());
    };
    let guard = handle
        .read()
        .map_err(|_| "context state handle lock poisoned".to_string())?;
    Ok(guard
        .0
        .list()
        .into_iter()
        .map(|ctx| ContextRow {
            id: ctx.id,
            name: ctx.name,
        })
        .collect())
}

fn read_bindings(state: &PluginStateRegistry) -> Result<Vec<ContextSessionBinding>, String> {
    let Some(handle) = state.get::<ContextStateHandle>() else {
        return Ok(Vec::new());
    };
    let guard = handle
        .read()
        .map_err(|_| "context state handle lock poisoned".to_string())?;
    Ok(guard
        .0
        .snapshot()
        .session_by_context
        .into_iter()
        .map(|(context_id, session_id)| ContextSessionBinding {
            context_id,
            session_id: session_id.0,
        })
        .collect())
}

// ── Typed state handle ───────────────────────────────────────────────

pub struct ControlCatalogStateHandle {
    caller: Arc<TypedServiceCaller>,
}

impl ControlCatalogStateHandle {
    const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }
}

impl control_catalog_state::ControlCatalogStateService for ControlCatalogStateHandle {
    fn snapshot<'a>(
        &'a self,
        since_revision: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Snapshot> + Send + 'a>> {
        Box::pin(async move {
            self.caller
                .call_service::<SnapshotArgs, Snapshot>(
                    bmux_control_catalog_plugin_api::capabilities::CATALOG_READ.as_str(),
                    ServiceKind::Query,
                    control_catalog_state::INTERFACE_ID.as_str(),
                    "snapshot",
                    &SnapshotArgs { since_revision },
                )
                .unwrap_or_else(|_| Snapshot {
                    revision: 0,
                    sessions: Vec::new(),
                    contexts: Vec::new(),
                    context_session_bindings: Vec::new(),
                })
        })
    }
}

impl bmux_plugin_sdk::definition::PluginInstance for ControlCatalogPlugin {
    fn activate(
        &self,
        context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        self.catalog.adopt_activation_bus(|| {
            bmux_plugin::take_activation_event_bus(&context.capabilities).ok()
        });
        self.adopt_activation_states(|| {
            bmux_plugin::take_activation_state_registry(&context.capabilities).ok()
        });
        Box::pin(async move { self.start_subscriber() })
    }
    fn deactivate(
        &self,
        _context: bmux_plugin_sdk::definition::PluginLifecycleContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'_, bmux_plugin_sdk::definition::PluginResult<()>>
    {
        Box::pin(async move { self.stop_subscriber() })
    }
    fn invoke(
        &self,
        call: bmux_plugin_sdk::definition::PluginCallContext,
        context: NativeServiceContext,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        '_,
        bmux_plugin_sdk::definition::PluginResult<ServiceResponse>,
    > {
        Box::pin(async move {
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable(
                    "catalog invocation cancelled",
                ));
            }
            Ok(self.service(&context))
        })
    }
}

// The one definition both bundling and generated dynamic packaging use:
// fresh default author state per created instance.
bmux_plugin_sdk::define_plugin!(
    rust_plugin = ControlCatalogPlugin,
    manifest = crate::declaration::PLUGIN_MANIFEST,
    roles = vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
);

/// Bind one host's neutral mechanisms before creating instances. The portable
/// adapter must translate its host handles into these same mechanisms; it must
/// not select the legacy global registry.
pub fn plugin_definition_with_host(
    events: Arc<EventBus>,
    state: Arc<PluginStateRegistry>,
) -> bmux_plugin_sdk::definition::PluginDefinition {
    bmux_plugin_sdk::definition::PluginDefinition::new(definition_metadata(), move |_scope| {
        Ok(Arc::new(ControlCatalogPlugin::with_host(
            Arc::clone(&events),
            Arc::clone(&state),
        ))
            as Arc<dyn bmux_plugin_sdk::definition::PluginInstance>)
    })
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn catalog_revisions_do_not_cross_hosts() {
        let first = ControlCatalogPlugin::with_host(
            Arc::new(EventBus::new()),
            Arc::new(PluginStateRegistry::new()),
        );
        let second = ControlCatalogPlugin::with_host(
            Arc::new(EventBus::new()),
            Arc::new(PluginStateRegistry::new()),
        );
        first.catalog.bump(vec![CatalogScope::Sessions], false);
        assert_eq!(first.build_snapshot(None).unwrap().revision, 2);
        assert_eq!(second.build_snapshot(None).unwrap().revision, 1);
    }

    #[test]
    fn subscriber_is_joined_before_reactivation() {
        let events = Arc::new(EventBus::new());
        events.register_channel::<SessionEvent>(sessions_events::EVENT_KIND);
        events.register_channel::<ContextEvent>(contexts_events::EVENT_KIND);
        events.register_channel::<ClientEvent>(clients_events::EVENT_KIND);
        let plugin = ControlCatalogPlugin::with_host(events, Arc::new(PluginStateRegistry::new()));
        plugin.start_subscriber().unwrap();
        assert!(plugin.start_subscriber().is_err());
        plugin.stop_subscriber().unwrap();
        assert!(plugin.subscriber.lock().unwrap().is_none());
        plugin.start_subscriber().unwrap();
        plugin.stop_subscriber().unwrap();
    }

    #[test]
    fn disconnected_catalog_is_not_a_healthy_empty_snapshot() {
        let plugin = ControlCatalogPlugin::with_host(
            Arc::new(EventBus::new()),
            Arc::new(PluginStateRegistry::new()),
        );
        plugin.catalog.disconnected.store(true, Ordering::Release);
        assert!(plugin.build_snapshot(None).is_err());
    }

    #[test]
    fn missing_dependency_channel_fails_activation_without_a_task() {
        let plugin = ControlCatalogPlugin::with_host(
            Arc::new(EventBus::new()),
            Arc::new(PluginStateRegistry::new()),
        );
        assert!(plugin.start_subscriber().is_err());
        assert!(plugin.subscriber.lock().unwrap().is_none());
    }
}

fn definition_metadata() -> bmux_plugin_sdk::definition::PluginMetadata {
    bmux_plugin_sdk::definition::PluginDefinition::from_rust_plugin::<ControlCatalogPlugin>(
        crate::declaration::PLUGIN_MANIFEST,
        vec![bmux_plugin_sdk::definition::ComponentRole::Backend],
    )
    .expect("plugin manifest must define valid canonical metadata")
    .metadata
}

#[cfg(test)]
mod canonical_metadata_tests {
    #[test]
    fn embedded_manifest_matches_definition_metadata() {
        let definition = bmux_plugin_sdk::definition::PluginDefinition::new(
            super::definition_metadata(),
            |_scope| {
                Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "metadata-only regression",
                ))
            },
        );
        definition
            .validate()
            .expect("canonical embedded identity must validate");
    }
}
