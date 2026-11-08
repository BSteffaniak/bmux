//! Host-owned callback admission and dependency ancestry; never payload authority.
#[cfg(test)]
#[path = "callbacks/tests.rs"]
mod tests;
use super::{Arc, InvocationScope, LoadedExecution, Mutex, ServiceCancellation};
use bmux_plugin_runtime::{AdmissionGuard, PluginInstanceId};
use bmux_plugin_sdk::PluginCommandError;
use bmux_plugin_sdk::callback_execution::{
    PluginBlockingCallback, PluginCallbackAuthority, PluginCallbackAuthoritySource,
    PluginCallbackContext, PluginCallbackEntryGuard, PluginCallbackExecution,
    PluginCallbackExecutor, PluginCallbackSettlement,
};
use bmux_plugin_sdk::definition::PluginResult;
use std::sync::Weak;

const MAX_ANCESTORS: usize = 32;
const MAX_ACCEPTED: usize = 8;
const MAX_WAITING: usize = 32;

fn rejected(message: impl Into<String>) -> PluginCommandError {
    PluginCommandError::unavailable(message)
}

/// One instance-local bounded executor owner, shared by its actual invocations.
pub(super) struct CallbackOwner {
    slots: Arc<tokio::sync::Semaphore>,
    subscriptions: Arc<tokio::sync::Semaphore>,
    waiting: std::sync::atomic::AtomicUsize,
}
impl Default for CallbackOwner {
    fn default() -> Self {
        Self {
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_ACCEPTED)),
            subscriptions: Arc::new(tokio::sync::Semaphore::new(128)),
            waiting: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

/// Opaque host-owned callback dependency. It cannot be built from resource IDs.
#[derive(Clone)]
pub struct CallbackEntryLease(Arc<Entry>);

/// Non-owning view of a callback entry. A nested call keeps only this to its
/// parent, so anything the callee retains after the call cannot keep the
/// parent invocation admitted.
#[derive(Clone)]
pub struct WeakCallbackEntryLease(std::sync::Weak<Entry>);
impl WeakCallbackEntryLease {
    /// The parent entry while it is still running; `None` once it has ended.
    #[must_use]
    pub fn upgrade(&self) -> Option<CallbackEntryLease> {
        self.0.upgrade().map(CallbackEntryLease)
    }
}

impl CallbackEntryLease {
    /// Observe this entry without extending its admission.
    #[must_use]
    pub fn downgrade(&self) -> WeakCallbackEntryLease {
        WeakCallbackEntryLease(Arc::downgrade(&self.0))
    }

    #[cfg(test)]
    pub(super) fn same_dependency(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    /// Validate this exact host-minted dependency against its selected binding.
    /// # Errors
    /// A different binding or a retired entry is never accepted by ID equality.
    pub fn validate_binding(&self, binding: &CallbackBinding) -> super::Result<()> {
        if !Arc::ptr_eq(&self.0.binding.0, &binding.0) {
            return Err(super::failure(
                "prepared callback entry belongs to another authority",
            ));
        }
        binding.validate()?;
        let current = binding
            .0
            .entry
            .lock()
            .map_err(|_| super::failure("callback entry poisoned"))?
            .upgrade()
            .ok_or_else(|| super::failure("prepared callback entry retired"))?;
        if !Arc::ptr_eq(&current, &self.0) {
            return Err(super::failure(
                "prepared callback entry has been superseded",
            ));
        }
        Ok(())
    }
    pub(super) fn has_dependents(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
    /// Whether this entry was admitted as a same-instance re-entry.
    #[must_use]
    pub fn is_reentrant(&self) -> bool {
        self.0.reentered
    }
}
struct Entry {
    binding: CallbackBinding,
    // Ancestry for re-entry detection only; weak so a retained child never
    // keeps its parent invocation admitted after the parent call ends.
    parent: Option<WeakCallbackEntryLease>,
    // A contract-approved same-instance re-entry: a live ancestor entry of
    // this instance exists, so instance-local admission must not wait.
    reentered: bool,
    _admission: AdmissionGuard,
}

/// Whether a child binding may enter an instance that already has a live
/// entry on its call chain. Selected by the host from provider declarations,
/// never from caller payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CallbackReentry {
    /// Default: any live same-instance ancestor rejects entry.
    #[default]
    Reject,
    /// The provider's BPDL contract marks this query `@reentrant`; the
    /// same-instance ancestor rejection alone is skipped.
    AllowQuery,
}

/// One actual instance's callback mechanism, reusable by static host adapters.
/// It grants no product permissions and never owns invocation retirement.
#[derive(Clone)]
pub struct LoaderCallbackHost(Arc<CallbackHostState>);
struct CallbackHostState {
    instance: bmux_plugin_runtime::InstanceScope,
    owner: Arc<CallbackOwner>,
    runtime: Option<tokio::runtime::Handle>,
    _code_pin: Arc<dyn std::any::Any + Send + Sync>,
}
impl LoaderCallbackHost {
    /// Retain once per actual selected instance, not once per call.
    /// `runtime` is the explicitly selected Rust-local reactor, never authority
    /// or an ABI value. The code pin must retain its runtime owner/driver until
    /// every accepted entry settles; a handle alone does not keep it running.
    #[must_use]
    pub fn new(
        instance: bmux_plugin_runtime::InstanceScope,
        code_pin: Arc<dyn std::any::Any + Send + Sync>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self(Arc::new(CallbackHostState {
            instance,
            owner: Arc::new(CallbackOwner::default()),
            runtime: Some(runtime),
            _code_pin: code_pin,
        }))
    }
    /// Bind actual host scopes before author execution; IDs alone confer nothing.
    /// # Errors
    /// Rejects foreign, closed or cancelled scopes.
    pub fn bind_root(
        &self,
        activation: &super::ActivationScope,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
    ) -> super::Result<CallbackBinding> {
        self.bind(
            activation,
            invocation,
            cancellation,
            None,
            CallbackReentry::Reject,
        )
    }
    /// Derive a nested dependency from its actual admitted parent lease.
    /// # Errors
    /// Rejects foreign runtime ancestry, closed parents and excessive depth.
    pub fn bind_child(
        &self,
        parent: &CallbackEntryLease,
        activation: &super::ActivationScope,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
    ) -> super::Result<CallbackBinding> {
        parent.0.binding.validate()?;
        if parent.0.binding.invocation().runtime_id() != invocation.runtime_id() {
            return Err(super::failure(
                "callback ancestry belongs to a different runtime",
            ));
        }
        let mut ancestor = Some(parent.clone());
        let mut depth = 0;
        while let Some(entry) = ancestor {
            depth += 1;
            if depth > MAX_ANCESTORS {
                return Err(super::failure("callback ancestry limit exceeded"));
            }
            ancestor = entry
                .0
                .parent
                .as_ref()
                .and_then(WeakCallbackEntryLease::upgrade);
        }
        self.bind(
            activation,
            invocation,
            cancellation,
            Some(parent.downgrade()),
            CallbackReentry::Reject,
        )
    }
    fn bind(
        &self,
        activation: &super::ActivationScope,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
        parent: Option<WeakCallbackEntryLease>,
        reentry: CallbackReentry,
    ) -> super::Result<CallbackBinding> {
        if self.0.instance.id() != invocation.instance_id()
            || self.0.instance.runtime_id() != invocation.runtime_id()
            || activation.instance_id() != self.0.instance.id()
            || activation.runtime_id() != self.0.instance.runtime_id()
            || activation.id() != invocation.activation_id().map_err(super::scope_error)?
        {
            return Err(super::failure(
                "callback scopes do not belong to selected instance/activation",
            ));
        }
        let admission = invocation.try_admit().map_err(super::scope_error)?;
        drop(admission);
        if cancellation.is_cancelled() {
            return Err(super::failure("callback request cancelled"));
        }
        Ok(CallbackBinding(Arc::new(Binding {
            execution: None,
            host: self.clone(),
            invocation,
            cancellation,
            parent,
            reentry,
            entry: Mutex::new(Weak::new()),
        })))
    }
}

/// Actual selected scope plus its immutable authenticated ancestors.
#[derive(Clone)]
pub struct CallbackBinding(Arc<Binding>);
struct Binding {
    execution: Option<Arc<LoadedExecution>>,
    host: LoaderCallbackHost,
    invocation: InvocationScope,
    cancellation: ServiceCancellation,
    parent: Option<WeakCallbackEntryLease>,
    reentry: CallbackReentry,
    entry: Mutex<Weak<Entry>>,
}

/// Selected host grants for one actual lifecycle invocation. No payload authority.
pub struct LifecycleCallbackDispatch {
    /// Optional actual-parent binding for nested lifecycle entry.
    pub callback: Option<CallbackBinding>,
    pub host: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
    pub kernel: Option<Arc<dyn crate::InvocationKernelDispatch>>,
}
/// Called before author entry with the loader's actual admitted lifecycle root.
pub type LifecycleCallbackFactory =
    dyn Fn(&CallbackBinding) -> super::Result<LifecycleCallbackDispatch> + Send + Sync;

impl LoadedExecution {
    /// Bind once before activation; retained for activation and cleanup roots.
    /// # Errors
    /// Rejects replacement or binding after activation has begun.
    pub fn bind_lifecycle_callback_factory(
        &self,
        factory: Arc<LifecycleCallbackFactory>,
    ) -> super::Result<()> {
        let _control = self.control()?;
        if self
            .state
            .lock()
            .map_err(|_| super::failure("execution state poisoned"))?
            .activation
            .is_some()
        {
            return Err(super::failure(
                "lifecycle callback factory must bind before activation",
            ));
        }
        let mut selected = self
            .lifecycle_callback_factory
            .lock()
            .map_err(|_| super::failure("lifecycle callback factory poisoned"))?;
        if selected.is_some() {
            return Err(super::failure("lifecycle callback factory already bound"));
        }
        *selected = Some(factory);
        drop(selected);
        Ok(())
    }
    /// Bind a host-authenticated root to an already admitted selected invocation.
    /// This carries no principal/origin grant; callers retain their server authority.
    /// # Errors
    /// Rejects foreign scopes and closed or cancelled admission.
    pub fn bind_root_callback(
        self: &Arc<Self>,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
    ) -> super::Result<CallbackBinding> {
        CallbackBinding::bind(
            self.clone(),
            invocation,
            cancellation,
            None,
            CallbackReentry::Reject,
        )
    }

    /// Derive a nested callback only from an actual live host entry dependency.
    /// # Errors
    /// Rejects foreign runtime ancestry, closed parents and exhausted depth.
    pub fn bind_child_callback(
        self: &Arc<Self>,
        parent: &CallbackEntryLease,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
    ) -> super::Result<CallbackBinding> {
        self.bind_child_callback_with(parent, invocation, cancellation, CallbackReentry::Reject)
    }

    /// Derive a nested callback with an explicit host-selected re-entry policy.
    ///
    /// [`CallbackReentry::AllowQuery`] must only be chosen for a query whose
    /// provider declaration marks the operation `@reentrant`; it skips the
    /// same-instance ancestor rejection and nothing else.
    /// # Errors
    /// Rejects foreign runtime ancestry, closed parents and exhausted depth.
    pub fn bind_child_callback_with(
        self: &Arc<Self>,
        parent: &CallbackEntryLease,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
        reentry: CallbackReentry,
    ) -> super::Result<CallbackBinding> {
        parent
            .0
            .binding
            .check()
            .map_err(|e| super::failure(e.message))?;
        if parent.0.binding.0.invocation.runtime_id() != invocation.runtime_id() {
            return Err(super::failure(
                "callback ancestry belongs to a different runtime",
            ));
        }
        let mut ancestor = Some(parent.clone());
        let mut depth = 0;
        while let Some(entry) = ancestor {
            depth += 1;
            if depth > MAX_ANCESTORS {
                return Err(super::failure("callback ancestry limit exceeded"));
            }
            ancestor = entry
                .0
                .parent
                .as_ref()
                .and_then(WeakCallbackEntryLease::upgrade);
        }
        CallbackBinding::bind(
            self.clone(),
            invocation,
            cancellation,
            Some(parent.downgrade()),
            reentry,
        )
    }
}
/// A host-authenticated dispatcher selector supplied by the owning runtime.
/// The lease is the actual active parent dependency, never caller payload data.
pub type CallbackHostDispatchFactory = dyn Fn(&CallbackEntryLease) -> super::Result<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>
    + Send
    + Sync;

struct EntryDispatch {
    binding: CallbackBinding,
    select: Arc<CallbackHostDispatchFactory>,
}
impl EntryDispatch {
    fn selected(
        &self,
        invocation: &InvocationScope,
    ) -> super::Result<(
        CallbackEntryLease,
        Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    )> {
        if invocation.id() != self.binding.invocation().id()
            || invocation.runtime_id() != self.binding.invocation().runtime_id()
        {
            return Err(super::failure("callback dispatcher invocation mismatch"));
        }
        let entry = self.binding.active_entry()?;
        let dispatch = (self.select)(&entry)?;
        Ok((entry, dispatch))
    }
}
impl bmux_plugin_sdk::HostServiceDispatch for EntryDispatch {
    fn invoke(
        &self,
        invocation: &InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> super::Result<Vec<u8>> {
        let (_entry, dispatch) = self.selected(invocation)?;
        dispatch.invoke(invocation, request)
    }
    fn open_retained_host(
        &self,
        invocation: &InvocationScope,
    ) -> super::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        let (_entry, dispatch) = self.selected(invocation)?;
        dispatch.open_retained_host(invocation)
    }
    fn open_retained_host_v2(
        &self,
        invocation: &InvocationScope,
    ) -> super::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        let (_entry, dispatch) = self.selected(invocation)?;
        dispatch.open_retained_host_v2(invocation)
    }
    fn invoke_async(
        &self,
        invocation: &InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, super::Result<Vec<u8>>> {
        let selected = self.selected(invocation);
        let invocation = invocation.clone();
        Box::pin(async move {
            let (_entry, dispatch) = selected?;
            dispatch.invoke_async(&invocation, request).await
        })
    }
    fn selected_service_availability(
        &self,
        invocation: &InvocationScope,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        'static,
        super::Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        let selected = self.selected(invocation);
        let invocation = invocation.clone();
        Box::pin(async move {
            let (_entry, dispatch) = selected?;
            dispatch
                .selected_service_availability(&invocation, key)
                .await
        })
    }
}

/// Host dispatch behind an activation's retained services.
///
/// A call on the activation's own lifecycle invocation made while the author
/// holds that invocation's callback entry goes through the entry-bound
/// dispatch, so it keeps the entry's ancestry exactly like any entered call.
/// Every other call (one outside the author entry, or on a fresh invocation
/// the activation services minted after setup) goes through the selected
/// instance dispatch for that invocation. Selection depends only on which
/// admitted invocation the call carries and whether its entry is live; a
/// failed call is never retried on the other route.
struct ActivationDispatch {
    binding: CallbackBinding,
    entered: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
    instance: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
}
impl ActivationDispatch {
    fn route(
        &self,
        invocation: &InvocationScope,
    ) -> super::Result<&Arc<dyn bmux_plugin_sdk::HostServiceDispatch>> {
        let own = invocation.id() == self.binding.invocation().id()
            && invocation.runtime_id() == self.binding.invocation().runtime_id();
        if own && self.binding.active_entry().is_ok() {
            return Ok(&self.entered);
        }
        self.instance
            .as_ref()
            .ok_or_else(|| super::failure("activation host dispatch not bound"))
    }
}
impl bmux_plugin_sdk::HostServiceDispatch for ActivationDispatch {
    fn invoke(
        &self,
        invocation: &InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> super::Result<Vec<u8>> {
        self.route(invocation)?.invoke(invocation, request)
    }
    fn open_retained_host(
        &self,
        invocation: &InvocationScope,
    ) -> super::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        self.route(invocation)?.open_retained_host(invocation)
    }
    fn open_retained_host_v2(
        &self,
        invocation: &InvocationScope,
    ) -> super::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        self.route(invocation)?.open_retained_host_v2(invocation)
    }
    fn invoke_async(
        &self,
        invocation: &InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, super::Result<Vec<u8>>> {
        match self.route(invocation) {
            Ok(dispatch) => dispatch.invoke_async(invocation, request),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
    fn selected_service_availability(
        &self,
        invocation: &InvocationScope,
        key: bmux_plugin_sdk::typed_dispatch::ServiceAvailabilityKey,
    ) -> bmux_plugin_sdk::definition::PluginFuture<
        'static,
        super::Result<bmux_plugin_sdk::typed_dispatch::SelectedServiceAvailability>,
    > {
        match self.route(invocation) {
            Ok(dispatch) => dispatch.selected_service_availability(invocation, key),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }
}

impl CallbackBinding {
    /// Dispatch for an activation's retained services: entry-bound while the
    /// author holds this lifecycle invocation's entry, the selected instance
    /// dispatch otherwise (see [`ActivationDispatch`]).
    pub(super) fn activation_dispatch(
        &self,
        entered: Arc<dyn bmux_plugin_sdk::HostServiceDispatch>,
        instance: Option<Arc<dyn bmux_plugin_sdk::HostServiceDispatch>>,
    ) -> Arc<dyn bmux_plugin_sdk::HostServiceDispatch> {
        Arc::new(ActivationDispatch {
            binding: self.clone(),
            entered,
            instance,
        })
    }

    /// Bind dispatch to the actual live author entry, including native callbacks.
    /// The selector preserves authenticated runtime caller/origin semantics.
    #[must_use]
    pub fn host_dispatch(
        &self,
        select: Arc<CallbackHostDispatchFactory>,
    ) -> Arc<dyn bmux_plugin_sdk::HostServiceDispatch> {
        Arc::new(EntryDispatch {
            binding: self.clone(),
            select,
        })
    }
    fn bind(
        execution: Arc<LoadedExecution>,
        invocation: InvocationScope,
        cancellation: ServiceCancellation,
        parent: Option<WeakCallbackEntryLease>,
        reentry: CallbackReentry,
    ) -> super::Result<Self> {
        let host = LoaderCallbackHost(Arc::new(CallbackHostState {
            instance: execution.scope.clone(),
            owner: execution.callback_owner.clone(),
            runtime: execution.callback_runtime.get().cloned(),
            _code_pin: Arc::new((
                execution.clone(),
                execution
                    .callback_runtime_owner
                    .lock()
                    .map_err(|_| super::failure("callback runtime owner poisoned"))?
                    .clone(),
            )),
        }));
        let mut binding = host.bind(
            &execution.activation()?,
            invocation,
            cancellation,
            parent,
            reentry,
        )?;
        Arc::get_mut(&mut binding.0)
            .expect("fresh callback binding has one owner")
            .execution = Some(execution);
        Ok(binding)
    }
    fn check(&self) -> PluginResult<()> {
        if self.0.cancellation.is_cancelled()
            || self
                .0
                .invocation
                .is_cancelled()
                .map_err(|e| rejected(e.to_string()))?
        {
            return Err(rejected("callback invocation cancelled"));
        }
        let admission = self
            .0
            .invocation
            .try_admit()
            .map_err(|e| rejected(e.to_string()))?;
        drop(admission);
        Ok(())
    }
    /// Capture the actual active dependency for a selected nested dispatch.
    /// # Errors
    /// Missing or settled entries cannot establish ancestry.
    pub fn active_entry(&self) -> super::Result<CallbackEntryLease> {
        self.check().map_err(|e| super::failure(e.message))?;
        self.0
            .entry
            .lock()
            .map_err(|_| super::failure("callback entry poisoned"))?
            .upgrade()
            .map(CallbackEntryLease)
            .ok_or_else(|| super::failure("callback has no live author entry"))
    }
    /// The selected executor and authority for this exact admitted invocation.
    #[must_use]
    pub fn context(&self) -> PluginCallbackContext {
        let authority = PluginCallbackAuthority::from_host(Arc::new(self.clone()));
        PluginCallbackContext::from_host(
            authority.clone(),
            PluginCallbackExecutor::from_host(Arc::new(SelectedExecutor {
                binding: self.clone(),
                authority,
            })),
        )
    }
    pub(super) fn validate(&self) -> super::Result<()> {
        self.check().map_err(|e| super::failure(e.message))
    }
    pub(super) fn subscription(&self) -> super::Result<tokio::sync::OwnedSemaphorePermit> {
        self.0
            .host
            .0
            .owner
            .subscriptions
            .clone()
            .try_acquire_owned()
            .map_err(|e| super::failure(e.to_string()))
    }
    pub(super) fn cancellation(&self) -> ServiceCancellation {
        self.0.cancellation.clone()
    }
    /// Actual invocation scope, not an author-supplied identity.
    #[must_use]
    pub fn invocation(&self) -> &InvocationScope {
        &self.0.invocation
    }
    pub(super) fn execution(&self) -> Option<&Arc<LoadedExecution>> {
        self.0.execution.as_ref()
    }
    pub(super) fn enter(&self) -> PluginResult<CallbackEntryLease> {
        self.check()?;
        let mut ancestor = self
            .0
            .parent
            .as_ref()
            .and_then(WeakCallbackEntryLease::upgrade);
        let mut reentered = false;
        while let Some(entry) = ancestor {
            if entry.0.binding.0.invocation.instance_id() == self.0.invocation.instance_id() {
                // Only a host-selected `@reentrant` query binding may enter
                // an instance with a live ancestor entry; every other check
                // (liveness, admission, single author entry) still applies.
                if self.0.reentry != CallbackReentry::AllowQuery {
                    return Err(rejected("recursive callback dependency rejected"));
                }
                reentered = true;
            }
            ancestor = entry
                .0
                .parent
                .as_ref()
                .and_then(WeakCallbackEntryLease::upgrade);
        }
        let admission = self
            .0
            .invocation
            .try_admit()
            .map_err(|e| rejected(e.to_string()))?;
        let mut active = self
            .0
            .entry
            .lock()
            .map_err(|_| rejected("callback entry poisoned"))?;
        if active.upgrade().is_some() {
            return Err(rejected("invocation already owns an author entry"));
        }
        let entry = Arc::new(Entry {
            binding: self.clone(),
            parent: self.0.parent.clone(),
            reentered,
            _admission: admission,
        });
        *active = Arc::downgrade(&entry);
        drop(active);
        Ok(CallbackEntryLease(entry))
    }
}
impl PluginCallbackAuthoritySource for CallbackBinding {
    fn check_live(&self) -> PluginResult<()> {
        self.check()
    }
    fn enter_adapter_entry(
        &self,
        instance: PluginInstanceId,
    ) -> PluginResult<PluginCallbackEntryGuard> {
        if instance != self.0.invocation.instance_id() {
            return Err(rejected("callback target is not the selected instance"));
        }
        self.enter().map(|entry| {
            let reentrant = entry.is_reentrant();
            let guard = PluginCallbackEntryGuard::from_host(entry);
            if reentrant {
                guard.into_reentrant()
            } else {
                guard
            }
        })
    }
    fn cancelled(&self) -> PluginCallbackSettlement {
        let binding = self.clone();
        Box::pin(async move {
            binding.check()?;
            binding
                .0
                .invocation
                .cancelled()
                .await
                .map_err(|e| rejected(e.to_string()))
        })
    }
}
struct Waiting(Arc<CallbackOwner>);
impl Drop for Waiting {
    fn drop(&mut self) {
        self.0
            .waiting
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
struct SelectedExecutor {
    binding: CallbackBinding,
    authority: PluginCallbackAuthority,
}
impl PluginCallbackExecution for SelectedExecutor {
    fn execute(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        let executor = Self {
            binding: self.binding.clone(),
            authority: self.authority.clone(),
        };
        Box::pin(async move {
            // Entry is acquired exactly once before any gate/worker queue.
            let entry = executor.authority.enter_adapter_entry(instance)?;
            entry.validate_for(&authority, instance)?;
            executor
                .execute_entered(authority, instance, entry, callback)
                .await
        })
    }
    fn execute_entered(
        &self,
        authority: PluginCallbackAuthority,
        instance: PluginInstanceId,
        entry: PluginCallbackEntryGuard,
        callback: PluginBlockingCallback,
    ) -> PluginCallbackSettlement {
        let binding = self.binding.clone();
        let selected = self.authority.clone();
        Box::pin(async move {
            entry.validate_for(&selected, instance)?;
            entry.validate_for(&authority, instance)?;
            binding.check()?;
            if instance != binding.0.invocation.instance_id() {
                return Err(rejected("callback executor instance mismatch"));
            }
            let owner = binding.0.host.0.owner.clone();
            let counted = owner
                .waiting
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            if counted >= MAX_WAITING {
                owner
                    .waiting
                    .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                return Err(rejected("callback queue limit exceeded"));
            }
            let waiting = Waiting(owner.clone());
            let permit = if entry.is_reentrant() {
                // A re-entrant entry's own ancestor occupies one of these
                // instance-local slots; waiting for capacity could block on
                // that ancestor forever, so admit now or fail explicitly.
                owner.slots.clone().try_acquire_owned().map_err(|_| {
                    rejected("reentrant call would deadlock on exhausted instance callback slots")
                })?
            } else {
                let mut acquire = std::pin::pin!(owner.slots.clone().acquire_owned());
                let mut cancelled = authority.cancelled();
                std::future::poll_fn(|cx| {
                    use std::future::Future;
                    if cancelled.as_mut().poll(cx).is_ready() {
                        return std::task::Poll::Ready(Err(rejected(
                            "callback cancelled before acceptance",
                        )));
                    }
                    acquire
                        .as_mut()
                        .poll(cx)
                        .map(|result| result.map_err(|e| rejected(e.to_string())))
                })
                .await?
            };
            drop(waiting);
            // Both the caller-supplied selected source and the executor's actual
            // scope must be live. Only this binding mints dependency ownership.
            entry.validate_for(&selected, instance)?;
            entry.validate_for(&authority, instance)?;
            let runtime = binding.0.host.0.runtime.clone().ok_or_else(|| {
                rejected("selected callback runtime unavailable; native reactor ownership is not projected")
            })?;
            let (sender, receiver) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("plugin-callback".into())
                .spawn(move || {
                    let _permit = permit;
                    let result = entry
                        .validate_for(&selected, instance)
                        .and_then(|()| entry.validate_for(&authority, instance))
                        .and_then(|()| binding.check())
                        .and_then(|()| {
                            // Runtime entry is execution plumbing, not authority.
                            // The actual prepared guard/code owner remains live.
                            let _runtime_entry = runtime.enter();
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(callback))
                                .unwrap_or_else(|_| Err(rejected("author callback panicked")))
                        });
                    drop(entry);
                    let _ = sender.send(result);
                })
                .map_err(|e| rejected(format!("callback worker admission failed: {e}")))?;
            receiver
                .await
                .map_err(|_| rejected("callback settlement unavailable"))?
        })
    }
}
