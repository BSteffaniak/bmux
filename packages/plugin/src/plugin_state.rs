//! Host-side shared state registry keyed by `TypeId`.
//!
//! `PluginStateRegistry` is a generic typemap. Plugins register an
//! `Arc<RwLock<T>>` during their `initialize` lifecycle; any code holding
//! a reference to the host can later look the state up by concrete type.
//!
//! This primitive lets plugins own domain state that the core runtime
//! (or other plugins) need to access directly, without introducing a
//! compile-time dependency from core crates on plugin crates. The
//! shared state type `T` must live in a neutral crate so both the
//! owner and consumer can name it — typically a plugin's `*-plugin-api`
//! crate.
//!
//! # Lifecycle
//!
//! 1. Plugin's `initialize` callback constructs its state and calls
//!    [`PluginStateRegistry::register`].
//! 2. After every foundational plugin has initialized, core code (or
//!    other plugins) may call [`PluginStateRegistry::get`] to obtain an
//!    `Arc<RwLock<T>>` handle.
//! 3. State remains alive for the lifetime of the registry (typically
//!    the server process). There is no deregister API — plugins are
//!    expected to live for the full server lifetime for foundational
//!    state roles.
//!
//! # Concurrency
//!
//! The registry itself is `RwLock`-protected. Individual state
//! entries are stored as opaque `Arc<dyn Any + Send + Sync>` and
//! downcast on lookup. Registering the same `TypeId` twice replaces
//! the earlier entry; callers should coordinate to avoid that.
//!
//! # Ownership
//!
//! There is no process-wide registry. The host program constructs one root
//! registry and hands each plugin activation a view from
//! [`PluginStateRegistry::scoped`] through the activation's capabilities
//! (see [`activation_binder`]). A view shares the root's entries. Handles a
//! view [`PluginStateRegistry::register`]s belong to its activation and are
//! removed when the view drops (unless replaced since); once the
//! activation scope closes the view admits no new registration. Shared
//! neutral state created with [`PluginStateRegistry::get_or_insert_with`] is
//! communal and never owned by the activation that happened to create it.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

type StateEntry = Arc<dyn Any + Send + Sync>;

/// Entries shared by a root registry and every view scoped from it.
#[derive(Default)]
struct StateCore {
    registrations: ActivationRegistrationRegistry,
    entries: RwLock<HashMap<TypeId, StateEntry>>,
}

/// The activation that owns a scoped view, and the handles it registered.
struct StateOwner {
    scope: ActivationScope,
    owned: Mutex<Vec<(TypeId, StateEntry)>>,
}

/// Shared-state registry keyed by concrete type: a host-owned root or an
/// activation-scoped view of one.
///
/// See module docs for the intended usage pattern.
pub struct PluginStateRegistry {
    core: Arc<StateCore>,
    owner: Option<StateOwner>,
}

impl Default for PluginStateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PluginStateRegistry {
    fn drop(&mut self) {
        let Some(owner) = &self.owner else {
            return;
        };
        let released = owner
            .owned
            .lock()
            .map(|mut registrations| std::mem::take(&mut *registrations))
            .unwrap_or_default();
        if let Ok(mut entries) = self.core.entries.write() {
            for (type_id, entry) in released {
                if entries
                    .get(&type_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &entry))
                {
                    entries.remove(&type_id);
                }
            }
        }
    }
}

impl std::fmt::Debug for PluginStateRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.core.entries.read().map_or(0, |g| g.len());
        f.debug_struct("PluginStateRegistry")
            .field("entries", &count)
            .field("scoped", &self.owner.is_some())
            .finish_non_exhaustive()
    }
}

impl PluginStateRegistry {
    /// Explicit per-host activation authority. All consumers of this host state
    /// share this registry rather than independently creating optional globals.
    #[must_use]
    pub fn registrations(&self) -> &ActivationRegistrationRegistry {
        &self.core.registrations
    }

    /// Create an empty root registry. The host program owns it and hands
    /// plugins scoped views; nothing else can reach it.
    #[must_use]
    pub fn new() -> Self {
        Self {
            core: Arc::default(),
            owner: None,
        }
    }

    /// A view of this registry owned by one plugin activation. See the
    /// module docs for what the activation owns.
    #[must_use]
    pub fn scoped(self: &Arc<Self>, scope: ActivationScope) -> Arc<Self> {
        Arc::new(Self {
            core: Arc::clone(&self.core),
            owner: Some(StateOwner {
                scope,
                owned: Mutex::new(Vec::new()),
            }),
        })
    }

    /// Whether `self` and `other` see the same entries.
    #[must_use]
    pub fn shares_entries_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.core, &other.core)
    }

    /// Shared observational lifecycle counter for this host registry.
    ///
    /// # Panics
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn shutdown_progress(&self) -> bmux_plugin_runtime::shutdown_progress::ShutdownProgress {
        self.get_or_insert_with(bmux_plugin_runtime::shutdown_progress::ShutdownProgress::default)
            .read()
            .expect("shutdown progress registry lock poisoned")
            .clone()
    }

    /// Register a state handle keyed by concrete type `T`.
    ///
    /// If a state of the same type was already registered it is
    /// replaced. Returns the previously-registered handle if any.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    ///
    /// Through a scoped view the handle belongs to the view's activation;
    /// once that activation scope has closed nothing is registered and
    /// `None` is returned.
    pub fn register<T>(&self, state: &Arc<RwLock<T>>) -> Option<Arc<RwLock<T>>>
    where
        T: Any + Send + Sync,
    {
        let _admission = match &self.owner {
            Some(owner) => Some(owner.scope.try_admit().ok()?),
            None => None,
        };
        let type_id = TypeId::of::<Arc<RwLock<T>>>();
        let boxed: StateEntry = Arc::new(state.clone());
        if let Some(owner) = &self.owner
            && let Ok(mut owned) = owner.owned.lock()
        {
            owned.push((type_id, Arc::clone(&boxed)));
        }
        let mut guard = self
            .core
            .entries
            .write()
            .expect("plugin state registry lock poisoned");
        let previous = guard.insert(type_id, boxed);
        drop(guard);
        previous.and_then(|arc| {
            arc.downcast::<Arc<RwLock<T>>>()
                .ok()
                .map(|boxed| (*boxed).clone())
        })
    }

    /// Publish shared neutral state that no activation owns, unless an entry
    /// of the same type already exists. Returns the entry now registered.
    ///
    /// Use this (or [`Self::get_or_insert_with`]) for communal state several
    /// plugins contribute to; [`Self::register`] would tie it to the
    /// registering activation.
    ///
    /// # Panics
    /// Panics if the registry lock is poisoned.
    pub fn register_communal<T>(&self, state: &Arc<RwLock<T>>) -> Arc<RwLock<T>>
    where
        T: Any + Send + Sync,
    {
        let shared = Arc::clone(state);
        self.get_or_insert_entry(move || shared)
    }

    fn get_or_insert_entry<T>(&self, initialize: impl FnOnce() -> Arc<RwLock<T>>) -> Arc<RwLock<T>>
    where
        T: Any + Send + Sync,
    {
        let mut entries = self
            .core
            .entries
            .write()
            .expect("plugin state registry lock poisoned");
        let entry = entries
            .entry(TypeId::of::<Arc<RwLock<T>>>())
            .or_insert_with(|| Arc::new(initialize()));
        let handle = entry
            .downcast_ref::<Arc<RwLock<T>>>()
            .expect("registered state type")
            .clone();
        drop(entries);
        handle
    }

    /// Retrieve or atomically initialize shared neutral state.
    ///
    /// # Panics
    /// Panics if the registry lock is poisoned. The initializer must not reenter it.
    #[must_use]
    pub fn get_or_insert_with<T>(&self, initialize: impl FnOnce() -> T) -> Arc<RwLock<T>>
    where
        T: Any + Send + Sync,
    {
        self.get_or_insert_entry(|| Arc::new(RwLock::new(initialize())))
    }

    /// Retrieve the registered state for concrete type `T` if any.
    ///
    /// # Panics
    ///
    /// Panics if the registry's internal lock is poisoned.
    #[must_use]
    pub fn get<T>(&self) -> Option<Arc<RwLock<T>>>
    where
        T: Any + Send + Sync,
    {
        let guard = self
            .core
            .entries
            .read()
            .expect("plugin state registry lock poisoned");
        let arc = guard.get(&TypeId::of::<Arc<RwLock<T>>>()).cloned();
        drop(guard);
        arc.and_then(|arc| arc.downcast::<Arc<RwLock<T>>>().ok())
            .map(|boxed| (*boxed).clone())
    }

    /// Retrieve the registered state for concrete type `T`, panicking
    /// if missing.
    ///
    /// Use this on core hot paths where the state is expected to have
    /// been registered during plugin initialization. Missing state
    /// indicates either a foundational plugin failed to load or a
    /// lookup occurred before initialization completed.
    ///
    /// # Panics
    ///
    /// Panics with a diagnostic message naming `T` if no state of
    /// type `T` has been registered.
    #[must_use]
    pub fn expect_state<T>(&self) -> Arc<RwLock<T>>
    where
        T: Any + Send + Sync,
    {
        self.get::<T>().unwrap_or_else(|| {
            panic!(
                "plugin state for `{}` not registered — foundational plugin missing or lookup too early",
                std::any::type_name::<T>()
            )
        })
    }

    /// Number of distinct state types currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.core.entries.read().map_or(0, |guard| guard.len())
    }

    /// `true` when no state is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Host activation binder that offers each activation a view of `registry`
/// scoped to that activation (see [`PluginStateRegistry::scoped`]).
#[must_use]
pub fn activation_binder(registry: Arc<PluginStateRegistry>) -> Arc<crate::ActivationBinder> {
    Arc::new(
        move |scope: &ActivationScope,
              capabilities: &bmux_plugin_sdk::definition::ActivationCapabilities| {
            capabilities
                .insert(ActivationStateRegistry(registry.scoped(scope.clone())))
                .map_err(|error| bmux_plugin_sdk::PluginError::ServiceProtocol {
                    details: error.message,
                })
        },
    )
}

/// The activation-scoped registry a host offers through [`activation_binder`].
pub struct ActivationStateRegistry(pub Arc<PluginStateRegistry>);

/// Take the state registry view the host supplied for this activation.
///
/// # Errors
/// Returns an explicit unavailable error when the host supplied none (for
/// example a dynamically loaded artifact) or it was already taken. There is
/// no process-wide fallback.
pub fn take_activation_state_registry(
    capabilities: &bmux_plugin_sdk::definition::ActivationCapabilities,
) -> Result<Arc<PluginStateRegistry>, bmux_plugin_sdk::PluginCommandError> {
    capabilities
        .take::<ActivationStateRegistry>()
        .map(|ActivationStateRegistry(registry)| registry)
}

/// Take a legacy author's activation registry, reporting at activation
/// time when the host supplied none. The plugin then works against an
/// empty registry of its own, never a process-wide one.
#[must_use]
pub fn activation_state_registry(
    context: &bmux_plugin_sdk::NativeLifecycleContext,
) -> Arc<PluginStateRegistry> {
    take_activation_state_registry(&context.capabilities).unwrap_or_else(|error| {
        tracing::warn!(
            plugin_id = %context.plugin_id,
            error = %error.message,
            "activation has no host state registry; using an empty private one"
        );
        Arc::new(PluginStateRegistry::new())
    })
}

// Explicit per-host registration ports. Unlike the legacy typemap above, these
// entries are keyed by contracts and cannot outlive their activation authority.
use bmux_plugin_runtime::{
    ActivationScope, AdmissionGuard, InvocationScope, LifecycleState, PluginActivationId,
    PluginInstanceId, PluginResourceId,
};
use bmux_plugin_sdk::{RegistrationContract, RegistrationKind};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

const MAX_REGISTRATIONS: usize = 1024;
const MAX_VALUE_BYTES: usize = 1024 * 1024;
const MAX_RETAINED_BYTES: usize = 16 * MAX_VALUE_BYTES;
const MAX_UPDATE_BATCHES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationError {
    InvalidContract,
    Duplicate,
    Capacity,
    Closed,
    NotActive,
    Missing,
    WrongKind,
    RevisionConflict,
    Lagged,
    Poisoned,
    Scope,
}

impl std::fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "plugin registration: {self:?}")
    }
}

impl std::error::Error for RegistrationError {}

/// A portable projection, never an executable capability. Possessing these IDs
/// does not grant registration, publication or callback authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationSnapshot {
    pub instance: PluginInstanceId,
    pub activation: PluginActivationId,
    pub resource: PluginResourceId,
    pub contract: RegistrationContract,
    pub revision: u64,
    pub value: Arc<[u8]>,
    pub ready: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationUpdate {
    Upsert(RegistrationSnapshot),
    Event(RegistrationSnapshot),
    Removed {
        instance: PluginInstanceId,
        activation: PluginActivationId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrationUpdateBatch {
    pub revision: u64,
    pub updates: Vec<RegistrationUpdate>,
}

struct RegistrationEntry {
    scope: ActivationScope,
    snapshot: RegistrationSnapshot,
    adapter: Option<Arc<dyn Any + Send + Sync>>,
    published: bool,
    withdrawn: bool,
}

struct RegistrationState {
    owners: BTreeMap<PluginActivationId, ActivationScope>,
    changed: tokio::sync::watch::Sender<u64>,
    revision: u64,
    entries: BTreeMap<(PluginActivationId, RegistrationContract), RegistrationEntry>,
    updates: VecDeque<RegistrationUpdateBatch>,
    journal_bytes: usize,
    retained_bytes: usize,
    closed: bool,
}

impl Default for RegistrationState {
    fn default() -> Self {
        Self {
            owners: BTreeMap::new(),
            changed: tokio::sync::watch::channel(0).0,
            revision: 0,
            entries: BTreeMap::new(),
            updates: VecDeque::new(),
            journal_bytes: 0,
            retained_bytes: 0,
            closed: false,
        }
    }
}

impl RegistrationState {
    fn next_revision(&self) -> Result<u64, RegistrationError> {
        self.revision
            .checked_add(1)
            .ok_or(RegistrationError::Capacity)
    }

    fn commit(&mut self, revision: u64, updates: Vec<RegistrationUpdate>) {
        self.revision = revision;
        self.changed.send_replace(revision);
        let bytes = updates
            .iter()
            .map(|update| match update {
                RegistrationUpdate::Upsert(snapshot) | RegistrationUpdate::Event(snapshot) => {
                    snapshot.value.len()
                }
                RegistrationUpdate::Removed { .. } => 0,
            })
            .sum::<usize>();
        self.journal_bytes += bytes;
        self.updates
            .push_back(RegistrationUpdateBatch { revision, updates });
        while self.updates.len() > MAX_UPDATE_BATCHES || self.journal_bytes > MAX_RETAINED_BYTES {
            if let Some(batch) = self.updates.pop_front() {
                for update in batch.updates {
                    if let RegistrationUpdate::Upsert(snapshot)
                    | RegistrationUpdate::Event(snapshot) = update
                    {
                        self.journal_bytes -= snapshot.value.len();
                    }
                }
            }
        }
    }
}

/// One registration registry per host.
///
/// Injected into static implementations and ABI adapters.
/// No process singleton is consulted. The bounded journal is a projection, not
/// durable storage. Lagged consumers must explicitly acquire a fresh snapshot.
#[derive(Clone, Default)]
pub struct ActivationRegistrationRegistry {
    state: Arc<Mutex<RegistrationState>>,
}

/// Trusted host-created capability. Never deserialize this from plugin payloads.
#[derive(Clone)]
pub struct ActivationRegistrationPort {
    registry: ActivationRegistrationRegistry,
    scope: ActivationScope,
}

/// Snapshot and cursor are acquired under the same lock as publication. No
/// update can fall between snapshot capture and subscription establishment.
pub struct RegistrationSubscription {
    registry: ActivationRegistrationRegistry,
    cursor: u64,
    changed: tokio::sync::watch::Receiver<u64>,
    disconnected: bool,
}

impl ActivationRegistrationRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Only the host supplies the actual owning activation scope.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn bind(
        &self,
        scope: ActivationScope,
    ) -> Result<ActivationRegistrationPort, RegistrationError> {
        let mut state = self.state.lock().map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        if !matches!(
            scope.state().map_err(|_| RegistrationError::Scope)?,
            LifecycleState::Activating | LifecycleState::Active
        ) {
            return Err(RegistrationError::Closed);
        }
        if !state.owners.contains_key(&scope.id()) && state.owners.len() >= MAX_REGISTRATIONS {
            return Err(RegistrationError::Capacity);
        }
        state
            .owners
            .entry(scope.id())
            .or_insert_with(|| scope.clone());
        drop(state);
        Ok(ActivationRegistrationPort {
            registry: self.clone(),
            scope,
        })
    }

    /// Resolve context from a host-issued live invocation, never a payload ID.
    /// Empty activations are bound too: service/registration access does not
    /// depend on already having published a contribution.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn context_for(
        &self,
        invocation: &InvocationScope,
    ) -> Result<ActivationRegistrationPort, RegistrationError> {
        let state = self.state.lock().map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        let activation = invocation
            .activation_id()
            .map_err(|_| RegistrationError::Scope)?;
        let scope = state
            .owners
            .get(&activation)
            .ok_or(RegistrationError::Missing)?;
        if scope.instance_id() != invocation.instance_id()
            || scope.runtime_id() != invocation.runtime_id()
        {
            return Err(RegistrationError::Scope);
        }
        let _guard = invocation
            .try_admit()
            .map_err(|_| RegistrationError::Closed)?;
        let scope = scope.clone();
        drop(state);
        Ok(ActivationRegistrationPort {
            registry: self.clone(),
            scope,
        })
    }

    /// Capture the existing typed service caller with explicit context-bound
    /// dispatch. This constructor is the scoped path: it cannot silently omit
    /// binding or use a library-global service bridge.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn service_caller(
        &self,
        invocation: InvocationScope,
        dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
        context: &bmux_plugin_sdk::NativeServiceContext,
    ) -> Result<crate::TypedServiceCaller, RegistrationError> {
        let owner = self.context_for(&invocation)?;
        let service = owner.bind_service(invocation, dispatch)?;
        Ok(
            crate::TypedServiceCaller::from_service_context(context)
                .with_host_service_port(service),
        )
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn subscribe(
        &self,
    ) -> Result<(Vec<RegistrationSnapshot>, RegistrationSubscription), RegistrationError> {
        let state = self.state.lock().map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        let snapshot = state
            .entries
            .values()
            .filter(|entry| entry.published)
            .map(|entry| entry.snapshot.clone())
            .collect();
        Ok((
            snapshot,
            RegistrationSubscription {
                registry: self.clone(),
                cursor: state.revision,
                changed: state.changed.subscribe(),
                disconnected: false,
            },
        ))
    }

    /// Obtain a safe static adapter only after lifecycle admission. The guard
    /// must be retained through the call; no plugin code runs under our lock.
    /// ABI adapters use this on the host side, not a serialized Rust pointer.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn admit<T: Any + Send + Sync>(
        &self,
        activation: PluginActivationId,
        contract: &RegistrationContract,
    ) -> Result<AdmittedRegistration<T>, RegistrationError> {
        let state = self.state.lock().map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        let entry = state
            .entries
            .get(&(activation, contract.clone()))
            .ok_or(RegistrationError::Missing)?;
        if !entry.published {
            return Err(RegistrationError::NotActive);
        }
        if entry.scope.state().map_err(|_| RegistrationError::Scope)? != LifecycleState::Active {
            return Err(RegistrationError::NotActive);
        }
        let adapter = entry
            .adapter
            .clone()
            .ok_or(RegistrationError::WrongKind)?
            .downcast::<T>()
            .map_err(|_| RegistrationError::WrongKind)?;
        let guard = entry
            .scope
            .try_admit()
            .map_err(|_| RegistrationError::Closed)?;
        drop(state);
        Ok(AdmittedRegistration {
            adapter,
            _guard: guard,
        })
    }

    /// Dispatch the checked adapter generated beside a typed provider. Host
    /// authorization is mandatory and occurs before effects on BOTH static and
    /// portable paths. The caller must retain this future as invocation-owned
    /// work until terminal settlement; dropping an observer is not cancellation.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub async fn dispatch_registered<A>(
        &self,
        invocation: &InvocationScope,
        provider: PluginActivationId,
        contract: &RegistrationContract,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
        stream: Option<bmux_plugin_sdk::ServiceStreamContext>,
        authorize: A,
    ) -> bmux_plugin_sdk::Result<Vec<u8>>
    where
        A: FnOnce(
            &InvocationScope,
            &RegistrationContract,
            &bmux_plugin_sdk::HostServiceRequest<'_>,
        ) -> bmux_plugin_sdk::Result<()>,
    {
        let error = |details: String| bmux_plugin_sdk::PluginError::ServiceProtocol { details };
        self.context_for(invocation)
            .map_err(|e| error(e.to_string()))?;
        if contract.kind != RegistrationKind::Service || contract.interface != request.interface {
            return Err(error("registered service contract mismatch".into()));
        }
        if request.payload.len() > MAX_VALUE_BYTES {
            return Err(error("registered service request exceeds bound".into()));
        }
        let _caller = invocation.try_admit().map_err(|e| error(e.to_string()))?;
        authorize(invocation, contract, &request)?;
        let admitted = self
            .admit::<bmux_plugin_sdk::TypedServiceHandle>(provider, contract)
            .map_err(|e| error(e.to_string()))?;
        if admitted.capability().as_str() != request.capability
            || admitted.interface_id().as_str() != request.interface
            || admitted.kind() != request.kind
        {
            return Err(error("registered service descriptor mismatch".into()));
        }
        let portable = admitted
            .portable_provider()
            .ok_or_else(|| error("generated portable service adapter unavailable".into()))?;
        let response = portable
            .dispatch_checked(request.operation, request.payload, stream)
            .await?;
        if response.len() > MAX_VALUE_BYTES {
            return Err(error(
                "registered service response exceeds bound; do not replay operation".into(),
            ));
        }
        Ok(response)
    }

    /// Disconnect observers and reject new binds/publications. Host teardown
    /// still has to withdraw each owner and drain/release its resources.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn close(&self) -> Result<(), RegistrationError> {
        let mut state = self.state.lock().map_err(|_| RegistrationError::Poisoned)?;
        state.closed = true;
        state.changed.send_replace(state.revision);
        drop(state);
        Ok(())
    }
}

/// Does not expose a clonable adapter Arc: callers cannot detach it from the
/// admission guard accidentally. Native plugins remain trusted code.
pub struct AdmittedRegistration<T> {
    adapter: Arc<T>,
    _guard: AdmissionGuard,
}

impl<T> std::ops::Deref for AdmittedRegistration<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.adapter
    }
}

impl ActivationRegistrationPort {
    #[must_use]
    pub const fn scope(&self) -> &ActivationScope {
        &self.scope
    }

    /// Bind a service capability to this activation's actual invocation. Host
    /// dispatch supplies authorization/provenance; payloads cannot substitute it.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn bind_service(
        &self,
        invocation: InvocationScope,
        dispatch: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    ) -> Result<bmux_plugin_sdk::BoundHostServicePort, RegistrationError> {
        let context = self.registry.context_for(&invocation)?;
        if context.scope.id() != self.scope.id() {
            return Err(RegistrationError::Scope);
        }
        Ok(bmux_plugin_sdk::BoundHostServicePort::new(
            invocation, dispatch,
        ))
    }

    /// Bind an application-granted kernel dispatcher to this activation's real
    /// invocation. This validates ownership, not foundational authorization; the
    /// supplied dispatcher must retain the application grant and authenticated
    /// context. Call only from trusted host wiring, never from plugin payload IDs.
    ///
    /// # Errors
    /// Rejects unbound, foreign or closed invocation owners.
    pub fn bind_kernel(
        &self,
        invocation: InvocationScope,
        dispatch: Arc<dyn crate::host_runtime::InvocationKernelDispatch>,
    ) -> Result<crate::host_runtime::InvocationKernelPort, RegistrationError> {
        let context = self.registry.context_for(&invocation)?;
        if context.scope.id() != self.scope.id() {
            return Err(RegistrationError::Scope);
        }
        Ok(crate::host_runtime::InvocationKernelPort::new(
            invocation, dispatch,
        ))
    }

    /// Cancellation-safe cleanup: withdrawal is idempotent and resource IDs
    /// remain retained if this future is dropped or drain/release fails. This
    /// deliberately does not finish the activation; M3 owns its lifecycle.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub async fn withdraw_and_drain(&self) -> Result<(), RegistrationError> {
        self.withdraw()?;
        self.scope
            .drain()
            .await
            .map_err(|_| RegistrationError::Scope)?;
        self.release_withdrawn()?;
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        state.owners.remove(&self.scope.id());
        drop(state);
        Ok(())
    }

    /// Register a generated typed+portable provider as one activation resource.
    /// The registration key explicitly includes query/command in its name to
    /// prevent accidental aliasing of two operations sharing an interface.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn register_service(
        &self,
        handle: bmux_plugin_sdk::TypedServiceHandle,
        version: u32,
    ) -> Result<(RegistrationContract, PluginResourceId), RegistrationError> {
        if handle.portable_provider().is_none() {
            return Err(RegistrationError::WrongKind);
        }
        let kind = match handle.kind() {
            bmux_plugin_sdk::ServiceKind::Query => "query",
            bmux_plugin_sdk::ServiceKind::Command => "command",
            bmux_plugin_sdk::ServiceKind::Event => return Err(RegistrationError::WrongKind),
        };
        let contract = RegistrationContract {
            kind: RegistrationKind::Service,
            interface: handle.interface_id().as_str().into(),
            version,
            name: format!("{kind}:{}", handle.capability().as_str()),
        };
        let resource = self.register(contract.clone(), Vec::new(), Some(Arc::new(handle)))?;
        Ok((contract, resource))
    }

    /// Register before publication. Static adapters may be small typed BPDL
    /// dispatch handles. Portable values contain only typed contract bytes.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn register(
        &self,
        contract: RegistrationContract,
        value: Vec<u8>,
        adapter: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Result<PluginResourceId, RegistrationError> {
        contract
            .validate()
            .map_err(|_| RegistrationError::InvalidContract)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(RegistrationError::Capacity);
        }
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        let key = (self.scope.id(), contract.clone());
        // Reject duplicates in the same owner and conflicting published OR
        // pending providers. Personal extension contributions use unique names.
        if state.entries.contains_key(&key)
            || state
                .entries
                .values()
                .any(|entry| !entry.withdrawn && entry.snapshot.contract == contract)
        {
            return Err(RegistrationError::Duplicate);
        }
        if state.entries.len() >= MAX_REGISTRATIONS
            || state.retained_bytes + value.len() > MAX_RETAINED_BYTES
        {
            return Err(RegistrationError::Capacity);
        }
        // Resource retention pins the adapter after withdrawal until the host
        // has drained work and explicitly releases it. IDs alone do not pin it.
        let owner = Arc::new(adapter.clone());
        let resource = self
            .scope
            .register_resource(owner)
            .map_err(|_| RegistrationError::Scope)?;
        state.retained_bytes += value.len();
        state.entries.insert(
            key,
            RegistrationEntry {
                scope: self.scope.clone(),
                snapshot: RegistrationSnapshot {
                    instance: self.scope.instance_id(),
                    activation: self.scope.id(),
                    resource,
                    contract,
                    revision: 0,
                    value: value.into(),
                    ready: false,
                },
                adapter,
                published: false,
                withdrawn: false,
            },
        );
        drop(state);
        Ok(resource)
    }

    /// Atomic publication of this owner's registrations, only after the host
    /// completes activation. Pending providers never appear in snapshots.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn publish(&self) -> Result<(), RegistrationError> {
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        if self.scope.state().map_err(|_| RegistrationError::Scope)? != LifecycleState::Active {
            return Err(RegistrationError::NotActive);
        }
        let _guard = self
            .scope
            .try_admit()
            .map_err(|_| RegistrationError::Closed)?;
        let revision = state.next_revision()?;
        let mut updates = Vec::new();
        for entry in state.entries.values_mut().filter(|entry| {
            entry.scope.id() == self.scope.id() && !entry.published && !entry.withdrawn
        }) {
            entry.published = true;
            entry.snapshot.revision = revision;
            updates.push(RegistrationUpdate::Upsert(entry.snapshot.clone()));
        }
        if !updates.is_empty() {
            state.commit(revision, updates);
        }
        drop(state);
        Ok(())
    }

    /// Compare against the authoritative retained revision, not a rendered cache.
    /// This does not acknowledge a durable write; durable state uses storage/BPDL.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn replace(
        &self,
        contract: &RegistrationContract,
        expected_revision: u64,
        value: Vec<u8>,
    ) -> Result<u64, RegistrationError> {
        if value.len() > MAX_VALUE_BYTES {
            return Err(RegistrationError::Capacity);
        }
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        if self.scope.state().map_err(|_| RegistrationError::Scope)? != LifecycleState::Active {
            return Err(RegistrationError::NotActive);
        }
        let _guard = self
            .scope
            .try_admit()
            .map_err(|_| RegistrationError::Closed)?;
        let key = (self.scope.id(), contract.clone());
        let entry = state.entries.get(&key).ok_or(RegistrationError::Missing)?;
        if !entry.published {
            return Err(RegistrationError::NotActive);
        }
        if contract.kind != RegistrationKind::RetainedState {
            return Err(RegistrationError::WrongKind);
        }
        if entry.snapshot.revision != expected_revision {
            return Err(RegistrationError::RevisionConflict);
        }
        let bytes = state.retained_bytes - entry.snapshot.value.len() + value.len();
        if bytes > MAX_RETAINED_BYTES {
            return Err(RegistrationError::Capacity);
        }
        let revision = state.next_revision()?;
        let entry = state
            .entries
            .get_mut(&key)
            .ok_or(RegistrationError::Missing)?;
        entry.snapshot.value = value.into();
        entry.snapshot.revision = revision;
        let update = RegistrationUpdate::Upsert(entry.snapshot.clone());
        state.retained_bytes = bytes;
        state.commit(revision, vec![update]);
        drop(state);
        Ok(revision)
    }

    /// Publish transient event bytes into the bounded update journal. Event
    /// payloads are never mistaken for retained state in a fresh snapshot.
    /// Slow consumers receive Lagged and must repair through domain contracts.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn emit(
        &self,
        contract: &RegistrationContract,
        value: Vec<u8>,
    ) -> Result<u64, RegistrationError> {
        if value.len() > MAX_VALUE_BYTES {
            return Err(RegistrationError::Capacity);
        }
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        if self.scope.state().map_err(|_| RegistrationError::Scope)? != LifecycleState::Active {
            return Err(RegistrationError::NotActive);
        }
        let _guard = self
            .scope
            .try_admit()
            .map_err(|_| RegistrationError::Closed)?;
        let entry = state
            .entries
            .get(&(self.scope.id(), contract.clone()))
            .ok_or(RegistrationError::Missing)?;
        if contract.kind != RegistrationKind::Event {
            return Err(RegistrationError::WrongKind);
        }
        if !entry.published {
            return Err(RegistrationError::NotActive);
        }
        let revision = state.next_revision()?;
        let mut event = entry.snapshot.clone();
        event.revision = revision;
        event.value = value.into();
        state.commit(revision, vec![RegistrationUpdate::Event(event)]);
        drop(state);
        Ok(revision)
    }

    /// A stale port cannot mark a replacement ready, even if logical plugin and
    /// signal names match. Readiness is not activation completion.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn mark_ready(&self, contract: &RegistrationContract) -> Result<(), RegistrationError> {
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            return Err(RegistrationError::Closed);
        }
        if !matches!(
            self.scope.state().map_err(|_| RegistrationError::Scope)?,
            LifecycleState::Activating | LifecycleState::Active
        ) {
            return Err(RegistrationError::NotActive);
        }
        let _guard = self
            .scope
            .try_admit()
            .map_err(|_| RegistrationError::Closed)?;
        let key = (self.scope.id(), contract.clone());
        let entry = state.entries.get(&key).ok_or(RegistrationError::Missing)?;
        if contract.kind != RegistrationKind::Readiness {
            return Err(RegistrationError::WrongKind);
        }
        if entry.withdrawn {
            return Err(RegistrationError::Closed);
        }
        if entry.snapshot.ready {
            return Ok(());
        }
        // Activation hooks may finish initial work before the host publishes.
        // Remember that readiness privately; no observer sees an inactive owner.
        if !entry.published {
            state
                .entries
                .get_mut(&key)
                .ok_or(RegistrationError::Missing)?
                .snapshot
                .ready = true;
            return Ok(());
        }
        let revision = state.next_revision()?;
        let entry = state
            .entries
            .get_mut(&key)
            .ok_or(RegistrationError::Missing)?;
        entry.snapshot.ready = true;
        entry.snapshot.revision = revision;
        let update = RegistrationUpdate::Upsert(entry.snapshot.clone());
        state.commit(revision, vec![update]);
        drop(state);
        Ok(())
    }

    /// Close admissions before withdrawal. Resource owners stay pinned in the
    /// lifecycle tree. After drain, call `release_withdrawn`; returned IDs are
    /// diagnostic cleanup evidence, not instructions to release independently.
    /// A cleanup failure must retain those IDs and report cleanup-required.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn withdraw(&self) -> Result<Vec<PluginResourceId>, RegistrationError> {
        self.scope.close().map_err(|_| RegistrationError::Scope)?;
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        let revision = state.next_revision()?;
        let mut resources = Vec::new();
        let mut published = false;
        for entry in state
            .entries
            .values_mut()
            .filter(|entry| entry.scope.id() == self.scope.id())
        {
            resources.push(entry.snapshot.resource);
            published |= entry.published;
            entry.published = false;
            entry.withdrawn = true;
        }
        if published {
            state.commit(
                revision,
                vec![RegistrationUpdate::Removed {
                    instance: self.scope.instance_id(),
                    activation: self.scope.id(),
                }],
            );
        }
        // Withdrawn entries remain retained for idempotent cleanup retry. They
        // count toward capacity until release succeeds; uncertainty is not GC.
        drop(state);
        Ok(resources)
    }

    /// Call after lifecycle drain. Failed release retains the entry for retry.
    /// Scope release independently checks outstanding admissions/children.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn release_withdrawn(&self) -> Result<(), RegistrationError> {
        let mut state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        let keys: Vec<_> = state
            .entries
            .iter()
            .filter(|(_, entry)| entry.scope.id() == self.scope.id() && entry.withdrawn)
            .map(|(key, _)| key.clone())
            .collect();
        // Never drop a plugin-owned adapter or scope resource under our lock,
        // including on partially successful cleanup followed by an error.
        let mut released = Vec::new();
        let mut failure = None;
        for key in keys {
            let resource = state.entries[&key].snapshot.resource;
            if let Ok(owner) = self.scope.release_resource(resource) {
                if let Some(entry) = state.entries.remove(&key) {
                    state.retained_bytes -= entry.snapshot.value.len();
                    released.push((owner, entry));
                }
            } else {
                failure = Some(RegistrationError::Scope);
                break;
            }
        }
        drop(state);
        drop(released);
        failure.map_or(Ok(()), Err)
    }
}

impl RegistrationSubscription {
    /// Await the next ordered batch without a polling timer. Receiver creation
    /// shares the snapshot lock, and watch retains changes between poll and
    /// await, so publication/close cannot lose a wakeup. Dropping this future
    /// does not advance a cursor unless a batch was actually returned.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub async fn next(&mut self) -> Result<Vec<RegistrationUpdateBatch>, RegistrationError> {
        loop {
            let updates = self.poll()?;
            if !updates.is_empty() {
                return Ok(updates);
            }
            self.changed
                .changed()
                .await
                .map_err(|_| RegistrationError::Closed)?;
        }
    }

    /// Poll without blocking the interactive thread. Empty means caught up, not
    /// completion. Host event loops can schedule polling; no unbounded queue.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn poll(&mut self) -> Result<Vec<RegistrationUpdateBatch>, RegistrationError> {
        if self.disconnected {
            return Err(RegistrationError::Closed);
        }
        let state = self
            .registry
            .state
            .lock()
            .map_err(|_| RegistrationError::Poisoned)?;
        if state.closed {
            self.disconnected = true;
            return Err(RegistrationError::Closed);
        }
        if self.cursor < state.revision
            && state
                .updates
                .front()
                .is_none_or(|batch| self.cursor < batch.revision - 1)
        {
            self.disconnected = true;
            return Err(RegistrationError::Lagged);
        }
        let updates = state
            .updates
            .iter()
            .filter(|batch| batch.revision > self.cursor)
            .cloned()
            .collect();
        self.cursor = state.revision;
        drop(state);
        Ok(updates)
    }
}

#[cfg(test)]
mod activation_registration_tests {
    use super::*;
    use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};

    fn owner(
        registry: &ActivationRegistrationRegistry,
    ) -> (ActivationScope, ActivationRegistrationPort) {
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let activation = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        let port = registry.bind(activation.clone()).unwrap();
        (activation, port)
    }

    fn contract(kind: RegistrationKind) -> RegistrationContract {
        RegistrationContract {
            kind,
            interface: "example/v1".into(),
            version: 1,
            name: "value".into(),
        }
    }

    #[tokio::test]
    async fn asynchronous_wait_observes_publication_and_close_without_lost_wake() {
        use std::{
            future::Future,
            pin::pin,
            task::{Context, Poll, Waker},
        };
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        let key = contract(RegistrationKind::RetainedState);
        port.register(key.clone(), vec![], None).unwrap();
        scope.mark_active().unwrap();
        port.publish().unwrap();
        let (_, mut subscription) = registry.subscribe().unwrap();
        {
            let mut waiting = pin!(subscription.next());
            assert!(matches!(
                waiting
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
            // Cancelling a waiting observer does not consume any updates.
        }
        let revision = registry.subscribe().unwrap().0[0].revision;
        port.replace(&key, revision, vec![4]).unwrap();
        assert_eq!(subscription.next().await.unwrap().len(), 1);
        registry.close().unwrap();
        assert_eq!(subscription.next().await, Err(RegistrationError::Closed));
        port.withdraw_and_drain().await.unwrap();
        scope.finish().unwrap();
    }

    #[tokio::test]
    async fn context_binding_is_host_local_and_cleanup_retires_empty_activation() {
        let a = ActivationRegistrationRegistry::new();
        let b = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&a);
        scope.mark_active().unwrap();
        let invocation = scope.begin_invocation().unwrap();
        assert_eq!(a.context_for(&invocation).unwrap().scope().id(), scope.id());
        assert!(matches!(
            b.context_for(&invocation),
            Err(RegistrationError::Missing)
        ));
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        assert!(a.context_for(&invocation).is_err());
        port.withdraw_and_drain().await.unwrap();
        scope.finish().unwrap();
    }

    #[tokio::test]
    async fn registered_service_authorization_failure_precedes_provider_lookup() {
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        scope.mark_active().unwrap();
        let invocation = scope.begin_invocation().unwrap();
        let key = contract(RegistrationKind::Service);
        let result = registry
            .dispatch_registered(
                &invocation,
                scope.id(),
                &key,
                bmux_plugin_sdk::HostServiceRequest {
                    capability: "example",
                    kind: bmux_plugin_sdk::ServiceKind::Query,
                    interface: "example/v1",
                    operation: "get",
                    payload: &[],
                },
                None,
                |_, _, _| {
                    Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "denied by host".into(),
                    })
                },
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("denied by host"));
        invocation.drain().await.unwrap();
        invocation.finish().unwrap();
        port.withdraw_and_drain().await.unwrap();
        scope.finish().unwrap();
    }

    #[test]
    fn snapshot_and_cursor_cannot_lose_an_update() {
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        let key = contract(RegistrationKind::RetainedState);
        port.register(key.clone(), vec![1], None).unwrap();
        assert_eq!(port.publish(), Err(RegistrationError::NotActive));
        assert_eq!(registry.subscribe().unwrap().0, Vec::new());
        scope.mark_active().unwrap();
        port.publish().unwrap();
        let (snapshot, mut subscription) = registry.subscribe().unwrap();
        assert_eq!(snapshot.len(), 1);
        let revision = port.replace(&key, snapshot[0].revision, vec![2]).unwrap();
        assert_eq!(
            port.replace(&key, snapshot[0].revision, vec![3]),
            Err(RegistrationError::RevisionConflict)
        );
        let batches = subscription.poll().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].revision, revision);
        assert_eq!(subscription.poll().unwrap(), Vec::new());
        port.withdraw().unwrap();
        port.release_withdrawn().unwrap();
    }

    #[test]
    fn readiness_during_activation_is_private_until_publication() {
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        let key = contract(RegistrationKind::Readiness);
        port.register(key.clone(), vec![], None).unwrap();
        port.mark_ready(&key).unwrap();
        assert_eq!(registry.subscribe().unwrap().0, Vec::new());
        scope.mark_active().unwrap();
        port.publish().unwrap();
        assert!(registry.subscribe().unwrap().0[0].ready);
        port.withdraw().unwrap();
        port.release_withdrawn().unwrap();
    }

    #[test]
    fn stale_owner_cannot_ready_replacement() {
        let registry = ActivationRegistrationRegistry::new();
        let key = contract(RegistrationKind::Readiness);
        let (first, stale) = owner(&registry);
        stale.register(key.clone(), vec![], None).unwrap();
        first.mark_active().unwrap();
        stale.publish().unwrap();
        stale.mark_ready(&key).unwrap();
        let resources = stale.withdraw().unwrap();
        assert_eq!(
            stale.withdraw().unwrap(),
            resources,
            "cleanup retry retains resource IDs"
        );
        let (second, current) = owner(&registry);
        current.register(key.clone(), vec![], None).unwrap();
        second.mark_active().unwrap();
        current.publish().unwrap();
        assert_eq!(stale.mark_ready(&key), Err(RegistrationError::NotActive));
        let snapshot = registry.subscribe().unwrap().0;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].activation, second.id());
        assert!(!snapshot[0].ready);
        stale.release_withdrawn().unwrap();
        current.withdraw().unwrap();
        current.release_withdrawn().unwrap();
    }

    #[test]
    fn duplicate_pending_provider_is_rejected_and_hosts_are_isolated() {
        let a = ActivationRegistrationRegistry::new();
        let b = ActivationRegistrationRegistry::new();
        let (_, first) = owner(&a);
        let (_, second) = owner(&a);
        let (_, independent) = owner(&b);
        let key = contract(RegistrationKind::Extension);
        first.register(key.clone(), vec![], None).unwrap();
        assert_eq!(
            second.register(key.clone(), vec![], None),
            Err(RegistrationError::Duplicate)
        );
        independent.register(key, vec![], None).unwrap();
        first.withdraw().unwrap();
        first.release_withdrawn().unwrap();
        second.withdraw().unwrap();
        independent.withdraw().unwrap();
        independent.release_withdrawn().unwrap();
    }

    #[test]
    fn lag_is_explicit_and_fresh_snapshot_repairs_retained_state() {
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        let key = contract(RegistrationKind::RetainedState);
        port.register(key.clone(), vec![], None).unwrap();
        scope.mark_active().unwrap();
        port.publish().unwrap();
        let (snapshot, mut slow) = registry.subscribe().unwrap();
        let mut revision = snapshot[0].revision;
        for _ in 0..=MAX_UPDATE_BATCHES {
            revision = port.replace(&key, revision, vec![9]).unwrap();
        }
        assert_eq!(slow.poll(), Err(RegistrationError::Lagged));
        assert_eq!(slow.poll(), Err(RegistrationError::Closed));
        assert_eq!(registry.subscribe().unwrap().0[0].revision, revision);
        port.withdraw().unwrap();
        port.release_withdrawn().unwrap();
    }

    #[test]
    fn adapter_stays_pinned_until_admission_drops_and_cleanup_retries() {
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        let key = contract(RegistrationKind::Service);
        port.register(key.clone(), vec![], Some(Arc::new(42_u32)))
            .unwrap();
        scope.mark_active().unwrap();
        port.publish().unwrap();
        let admitted = registry.admit::<u32>(scope.id(), &key).unwrap();
        assert_eq!(*admitted, 42);
        port.withdraw().unwrap();
        assert_eq!(port.release_withdrawn(), Err(RegistrationError::Scope));
        assert_eq!(
            port.register(key.clone(), vec![], None),
            Err(RegistrationError::Duplicate)
        );
        assert!(matches!(
            registry.admit::<u32>(scope.id(), &key),
            Err(RegistrationError::NotActive)
        ));
        drop(admitted);
        port.release_withdrawn().unwrap();
        assert!(matches!(
            registry.admit::<u32>(scope.id(), &key),
            Err(RegistrationError::Missing)
        ));
    }

    #[test]
    fn transient_event_is_not_replayed_as_snapshot_state() {
        let registry = ActivationRegistrationRegistry::new();
        let (scope, port) = owner(&registry);
        let key = contract(RegistrationKind::Event);
        port.register(key.clone(), vec![], None).unwrap();
        scope.mark_active().unwrap();
        port.publish().unwrap();
        let (_, mut stream) = registry.subscribe().unwrap();
        port.emit(&key, vec![7]).unwrap();
        assert!(
            matches!(&stream.poll().unwrap()[0].updates[0], RegistrationUpdate::Event(event) if event.value.as_ref() == [7])
        );
        assert!(registry.subscribe().unwrap().0[0].value.is_empty());
        registry.close().unwrap();
        assert_eq!(stream.poll(), Err(RegistrationError::Closed));
        assert_eq!(port.emit(&key, vec![8]), Err(RegistrationError::Closed));
        port.withdraw().unwrap();
        port.release_withdrawn().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActivationStateRegistry, PluginStateRegistry, activation_binder,
        take_activation_state_registry,
    };
    use std::sync::{Arc, RwLock};

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Counter {
        value: u32,
    }

    #[derive(Debug, Default)]
    struct Other {
        flag: bool,
    }

    #[test]
    fn registers_and_retrieves_state() {
        let reg = PluginStateRegistry::new();
        let state = Arc::new(RwLock::new(Counter { value: 42 }));
        assert!(reg.register(&state).is_none());

        let retrieved = reg.get::<Counter>().expect("state not found");
        assert_eq!(retrieved.read().unwrap().value, 42);

        retrieved.write().unwrap().value = 99;
        let retrieved2 = reg.get::<Counter>().expect("state not found");
        assert_eq!(retrieved2.read().unwrap().value, 99);
    }

    #[test]
    fn re_register_replaces_entry_and_returns_previous() {
        let reg = PluginStateRegistry::new();
        let first = Arc::new(RwLock::new(Counter { value: 1 }));
        let second = Arc::new(RwLock::new(Counter { value: 2 }));

        assert!(reg.register(&first).is_none());
        let previous = reg.register(&second).expect("expected previous");
        assert_eq!(previous.read().unwrap().value, 1);
        assert_eq!(reg.get::<Counter>().unwrap().read().unwrap().value, 2);
    }

    #[test]
    fn different_types_are_independent() {
        let reg = PluginStateRegistry::new();
        let counter = Arc::new(RwLock::new(Counter { value: 7 }));
        let other = Arc::new(RwLock::new(Other { flag: true }));
        reg.register(&counter);
        reg.register(&other);
        assert_eq!(reg.get::<Counter>().unwrap().read().unwrap().value, 7);
        assert!(reg.get::<Other>().unwrap().read().unwrap().flag);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn missing_state_returns_none() {
        let reg = PluginStateRegistry::new();
        assert!(reg.get::<Counter>().is_none());
    }

    #[test]
    #[should_panic(expected = "not registered")]
    fn expect_panics_when_missing() {
        let reg = PluginStateRegistry::new();
        let _ = reg.expect_state::<Counter>();
    }

    #[test]
    fn concurrent_access_is_safe() {
        use std::thread;

        let reg = Arc::new(PluginStateRegistry::new());
        let counter = Arc::new(RwLock::new(Counter { value: 0 }));
        reg.register(&counter);

        let mut handles = vec![];
        for _ in 0..8 {
            let reg = reg.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let state = reg.get::<Counter>().unwrap();
                    state.write().unwrap().value += 1;
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let final_value = reg.get::<Counter>().unwrap().read().unwrap().value;
        assert_eq!(final_value, 8 * 1000);
    }

    fn activation() -> bmux_plugin_runtime::ActivationScope {
        bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()),
        )
        .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
        .expect("activation")
    }

    #[test]
    fn independent_roots_never_share_entries() {
        let a = PluginStateRegistry::new();
        let b = PluginStateRegistry::new();
        assert!(!a.shares_entries_with(&b));
        a.register(&Arc::new(RwLock::new(Counter { value: 1 })));
        assert!(b.get::<Counter>().is_none());
    }

    #[test]
    fn view_owns_registrations_but_not_communal_state() {
        let root = Arc::new(PluginStateRegistry::new());
        let view = root.scoped(activation());
        assert!(view.shares_entries_with(&root));
        view.register(&Arc::new(RwLock::new(Counter { value: 7 })));
        let communal = view.get_or_insert_with(Other::default);
        communal.write().unwrap().flag = true;
        let shared = view.register_communal(&Arc::new(RwLock::new(0_u64)));
        *shared.write().unwrap() = 3;
        assert_eq!(root.get::<Counter>().unwrap().read().unwrap().value, 7);
        drop(view);
        assert!(
            root.get::<Counter>().is_none(),
            "owned entry leaves with its view"
        );
        assert!(root.get::<Other>().unwrap().read().unwrap().flag);
        assert_eq!(*root.get::<u64>().unwrap().read().unwrap(), 3);
    }

    #[test]
    fn view_never_removes_a_newer_registration_and_closed_view_registers_nothing() {
        let root = Arc::new(PluginStateRegistry::new());
        let first = root.scoped(activation());
        let scope = activation();
        let second = root.scoped(scope.clone());
        first.register(&Arc::new(RwLock::new(Counter { value: 1 })));
        second.register(&Arc::new(RwLock::new(Counter { value: 2 })));
        drop(first);
        assert_eq!(root.get::<Counter>().unwrap().read().unwrap().value, 2);
        scope.close().unwrap();
        assert!(
            second
                .register(&Arc::new(RwLock::new(Other::default())))
                .is_none()
        );
        assert!(root.get::<Other>().is_none());
    }

    #[test]
    fn activation_binder_offers_each_activation_its_own_view() {
        let root = Arc::new(PluginStateRegistry::new());
        let binder = activation_binder(Arc::clone(&root));
        let take = || {
            let capabilities = bmux_plugin_sdk::definition::ActivationCapabilities::default();
            binder(&activation(), &capabilities).unwrap();
            let view = take_activation_state_registry(&capabilities).unwrap();
            assert!(capabilities.take::<ActivationStateRegistry>().is_err());
            view
        };
        let (a, b) = (take(), take());
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(a.shares_entries_with(&root) && b.shares_entries_with(&root));
    }
}
