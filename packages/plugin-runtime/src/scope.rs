//! Instance, activation and invocation ownership with atomic admission fencing.
//!
//! A tree has one short-held metadata lock. No plugin code runs under it. Closing
//! and admission linearize under that lock; an admission increments every ancestor
//! before releasing it. Drain uses a watch revision subscribed before inspecting
//! the predicate, so completion between inspection and suspension cannot be lost.
use std::{
    any::Any,
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::watch;

use crate::{
    PluginActivationId, PluginConcurrencyConfig, PluginInstanceId, PluginResourceId,
    PluginRuntimeScopeId, RuntimeInvocationId, background::BackgroundTasks,
    background::TaskCancellation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Created,
    Activating,
    Active,
    Closing,
    Inactive,
    Destroyed,
    CleanupRequired,
}

/// Explicit trusted-host callback authority for one lifecycle operation.
/// This is not a service admission mode and never changes ancestor phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleInvocationKind {
    Activation,
    Cleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("scope is closed or no longer retained")]
    Closed,
    #[error("invalid lifecycle transition")]
    InvalidTransition,
    #[error("scope concurrency limit reached")]
    Busy,
    #[error("scope capacity reached")]
    Capacity,
    #[error("scope has outstanding admissions, children, tasks, or resources")]
    Outstanding,
    #[error("scope state lock poisoned; cleanup authority retained")]
    Poisoned,
    #[error("scope cleanup failed: {0}")]
    Cleanup(String),
    #[error("resource does not belong to this scope")]
    UnknownResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Instance,
    Activation(PluginActivationId),
    Invocation(RuntimeInvocationId),
}

struct Node {
    parent: Option<Key>,
    phase: LifecycleState,
    admitted: usize,
    direct: usize,
    policy: PluginConcurrencyConfig,
    cancellation: watch::Sender<bool>,
    tasks: BackgroundTasks,
    tasks_drained: bool,
    resources: BTreeMap<PluginResourceId, Arc<dyn Any + Send + Sync>>,
    failure: Option<String>,
    lifecycle_authority: Option<LifecycleInvocationKind>,
}

impl Node {
    fn new(parent: Option<Key>, phase: LifecycleState, policy: PluginConcurrencyConfig) -> Self {
        Self {
            parent,
            phase,
            admitted: 0,
            direct: 0,
            policy,
            cancellation: watch::channel(false).0,
            tasks: BackgroundTasks::new(),
            tasks_drained: true,
            resources: BTreeMap::new(),
            failure: None,
            lifecycle_authority: None,
        }
    }

    fn accepts(&self) -> bool {
        matches!(
            self.phase,
            LifecycleState::Activating | LifecycleState::Active
        )
    }
}

struct Tree {
    runtime: PluginRuntimeScopeId,
    instance: PluginInstanceId,
    nodes: Mutex<BTreeMap<Key, Node>>,
    changed: watch::Sender<()>,
    // Deliberate retention until explicit successful instance finish. Losing all
    // observers must leak safely rather than unload code needed by detached work.
    retained: Mutex<Option<Arc<Tree>>>,
}

impl Tree {
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<Key, Node>>, ScopeError> {
        self.nodes.lock().map_err(|_| ScopeError::Poisoned)
    }
}

#[derive(Clone)]
struct Scope {
    tree: Arc<Tree>,
    key: Key,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Scope")
            .field("runtime", &self.tree.runtime)
            .field("instance", &self.tree.instance)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

fn ancestry(nodes: &BTreeMap<Key, Node>, mut key: Key) -> Result<Vec<Key>, ScopeError> {
    let mut keys = Vec::with_capacity(3);
    loop {
        let node = nodes.get(&key).ok_or(ScopeError::Closed)?;
        keys.push(key);
        let Some(parent) = node.parent else {
            return Ok(keys);
        };
        key = parent;
    }
}

/// Check the whole path under the same lock used to admit. Only an explicitly
/// minted lifecycle root can cross its activation/instance phase boundary; no
/// closed invocation (including that root itself) can ever be bypassed.
fn accepts_path(nodes: &BTreeMap<Key, Node>, keys: &[Key]) -> bool {
    let authority = keys.iter().find_map(|key| nodes[key].lifecycle_authority);
    keys.iter().all(|key| {
        let node = &nodes[key];
        match (authority, key) {
            (Some(LifecycleInvocationKind::Activation), Key::Activation(_) | Key::Instance) => {
                node.phase == LifecycleState::Activating
            }
            (Some(LifecycleInvocationKind::Cleanup), Key::Activation(_)) => matches!(
                node.phase,
                LifecycleState::Closing | LifecycleState::CleanupRequired
            ),
            (Some(LifecycleInvocationKind::Cleanup), Key::Instance) => matches!(
                node.phase,
                LifecycleState::Activating
                    | LifecycleState::Active
                    | LifecycleState::Closing
                    | LifecycleState::CleanupRequired
            ),
            _ => node.accepts(),
        }
    })
}

fn limit(policy: PluginConcurrencyConfig) -> usize {
    match policy {
        PluginConcurrencyConfig::Concurrent => usize::MAX,
        PluginConcurrencyConfig::Exclusive => 1,
        PluginConcurrencyConfig::Limited { max } => max.get(),
    }
}

impl Scope {
    fn phase(&self) -> Result<LifecycleState, ScopeError> {
        Ok(self
            .tree
            .lock()?
            .get(&self.key)
            .ok_or(ScopeError::Closed)?
            .phase)
    }

    fn admit(&self) -> Result<AdmissionGuard, ScopeError> {
        let mut nodes = self.tree.lock()?;
        let keys = ancestry(&nodes, self.key)?;
        if !accepts_path(&nodes, &keys) {
            return Err(ScopeError::Closed);
        }
        for key in &keys {
            let node = &nodes[key];
            if node.admitted == usize::MAX {
                return Err(ScopeError::Capacity);
            }
        }
        let node = &nodes[&self.key];
        if node.direct >= 1024 {
            return Err(ScopeError::Capacity);
        }
        for key in keys {
            nodes.get_mut(&key).ok_or(ScopeError::Closed)?.admitted += 1;
        }
        nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?.direct += 1;
        Ok(AdmissionGuard {
            scope: self.clone(),
        })
    }

    fn close(&self) -> Result<(), ScopeError> {
        let mut nodes = self.tree.lock()?;
        if !nodes.contains_key(&self.key) {
            return Err(ScopeError::Closed);
        }
        let affected: Vec<_> = nodes
            .keys()
            .copied()
            .filter(|key| ancestry(&nodes, *key).is_ok_and(|keys| keys.contains(&self.key)))
            .collect();
        let mut tasks = Vec::with_capacity(affected.len());
        for key in affected {
            let node = nodes.get_mut(&key).ok_or(ScopeError::Closed)?;
            if !matches!(
                node.phase,
                LifecycleState::Destroyed | LifecycleState::CleanupRequired
            ) {
                node.phase = LifecycleState::Closing;
            }
            node.cancellation.send_replace(true);
            tasks.push(node.tasks.clone());
        }
        self.tree.changed.send_replace(());
        drop(nodes);
        // Never nest task and tree locks: failed task admission may drop a
        // captured AdmissionGuard while BackgroundTasks still holds its lock.
        for task_scope in tasks {
            task_scope.cancel().map_err(ScopeError::Cleanup)?;
        }
        Ok(())
    }

    async fn drain(&self) -> Result<(), ScopeError> {
        self.close()?;
        let tasks = self
            .tree
            .lock()?
            .get(&self.key)
            .ok_or(ScopeError::Closed)?
            .tasks
            .clone();
        let outcome = tasks.shutdown_retained(None).await;
        {
            let mut nodes = self.tree.lock()?;
            let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
            node.tasks_drained = true;
            if let Err(error) = outcome {
                node.phase = LifecycleState::CleanupRequired;
                node.failure = Some(error.clone());
                return Err(ScopeError::Cleanup(error));
            }
        }
        let mut changed = self.tree.changed.subscribe();
        loop {
            {
                let nodes = self.tree.lock()?;
                let node = nodes.get(&self.key).ok_or(ScopeError::Closed)?;
                if node.admitted == 0 {
                    return node
                        .failure
                        .clone()
                        .map_or(Ok(()), |failure| Err(ScopeError::Cleanup(failure)));
                }
            }
            changed.changed().await.map_err(|_| ScopeError::Closed)?;
        }
    }

    fn spawn<F, Fut>(&self, name: &str, work: F) -> Result<(), ScopeError>
    where
        F: FnOnce(TaskCancellation) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let admission = self.admit()?;
        let tasks = {
            let mut nodes = self.tree.lock()?;
            let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
            node.tasks_drained = false;
            node.tasks.clone()
        };
        // Factory execution is inside the task, not inside BackgroundTasks' lock.
        tasks
            .spawn(name, move |cancel| async move {
                let _admission = admission;
                work(cancel).await
            })
            .map_err(ScopeError::Cleanup)
    }

    fn register<T: Any + Send + Sync>(
        &self,
        resource: Arc<T>,
    ) -> Result<PluginResourceId, ScopeError> {
        let mut nodes = self.tree.lock()?;
        let keys = ancestry(&nodes, self.key)?;
        let created_instance =
            self.key == Key::Instance && nodes[&self.key].phase == LifecycleState::Created;
        if !created_instance && !accepts_path(&nodes, &keys) {
            return Err(ScopeError::Closed);
        }
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
        if node.resources.len() >= 1024 {
            return Err(ScopeError::Capacity);
        }
        let id = PluginResourceId::fresh();
        node.resources.insert(id, resource);
        Ok(id)
    }

    fn release(
        &self,
        resource: PluginResourceId,
    ) -> Result<Arc<dyn Any + Send + Sync>, ScopeError> {
        let mut nodes = self.tree.lock()?;
        if nodes.values().any(|child| child.parent == Some(self.key)) {
            return Err(ScopeError::Outstanding);
        }
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
        if node.admitted != 0 {
            return Err(ScopeError::Outstanding);
        }
        // Return ownership, so a plugin-owned destructor never runs under lock.
        node.resources
            .remove(&resource)
            .ok_or(ScopeError::UnknownResource)
    }

    fn finish(&self) -> Result<(), ScopeError> {
        let mut nodes = self.tree.lock()?;
        let node = nodes.get(&self.key).ok_or(ScopeError::Closed)?;
        if let Some(failure) = &node.failure {
            return Err(ScopeError::Cleanup(failure.clone()));
        }
        if node.phase != LifecycleState::Closing {
            return Err(ScopeError::InvalidTransition);
        }
        if node.admitted != 0
            || !node.tasks_drained
            || !node.resources.is_empty()
            || nodes.values().any(|child| child.parent == Some(self.key))
        {
            return Err(ScopeError::Outstanding);
        }
        match self.key {
            Key::Instance => {
                nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?.phase =
                    LifecycleState::Destroyed;
            }
            Key::Activation(_) => {
                nodes.remove(&self.key);
                let parent = nodes.get_mut(&Key::Instance).ok_or(ScopeError::Closed)?;
                if matches!(
                    parent.phase,
                    LifecycleState::Active | LifecycleState::Activating
                ) {
                    parent.phase = LifecycleState::Inactive;
                }
            }
            Key::Invocation(_) => {
                nodes.remove(&self.key);
            }
        }
        drop(nodes);
        if self.key == Key::Instance {
            self.tree
                .retained
                .lock()
                .map_err(|_| ScopeError::Poisoned)?
                .take();
        }
        self.tree.changed.send_replace(());
        Ok(())
    }

    fn resolve_cleanup(&self) -> Result<(), ScopeError> {
        let mut nodes = self.tree.lock()?;
        if nodes.values().any(|child| child.parent == Some(self.key)) {
            return Err(ScopeError::Outstanding);
        }
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
        if node.phase != LifecycleState::CleanupRequired {
            return Err(ScopeError::InvalidTransition);
        }
        if node.admitted != 0 || !node.tasks_drained || !node.resources.is_empty() {
            return Err(ScopeError::Outstanding);
        }
        // Explicit host acknowledgement, never an automatic retry/reset. The
        // zero-admission predicate proves no task factory/admission destructor
        // can need the tree lock, so this empty-task acknowledgement is safe to
        // serialize here against new failure reports.
        node.tasks
            .acknowledge_failures()
            .map_err(ScopeError::Cleanup)?;
        node.failure = None;
        node.phase = LifecycleState::Closing;
        Ok(())
    }

    fn fail_cleanup(&self, error: String) -> Result<(), ScopeError> {
        self.close()?;
        let mut nodes = self.tree.lock()?;
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
        node.phase = LifecycleState::CleanupRequired;
        node.failure = Some(error);
        Ok(())
    }
}

/// One admitted operation. Holding this guard, not possessing an ID, grants
/// callback lifetime. It never holds a callback-state mutex across nested work.
#[derive(Debug)]
pub struct AdmissionGuard {
    scope: Scope,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        // Poison remains an explicit error on all public operations. Recover only
        // to decrement retained counts, never to resume admission after a panic.
        let mut nodes = self
            .scope
            .tree
            .nodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(keys) = ancestry(&nodes, self.scope.key) {
            for key in keys {
                if let Some(node) = nodes.get_mut(&key) {
                    node.admitted -= 1;
                }
            }
            if let Some(node) = nodes.get_mut(&self.scope.key) {
                node.direct -= 1;
            }
        }
        self.scope.tree.changed.send_replace(());
    }
}

/// Independently owned instance; one activation at a time.
#[derive(Debug, Clone)]
pub struct InstanceScope(Scope);
/// Owns registration, subscription and producer resources for one activation.
#[derive(Debug, Clone)]
pub struct ActivationScope(Scope);
/// Owns invocation callbacks and child tasks. Copies are handles, not admissions.
#[derive(Debug, Clone)]
pub struct InvocationScope(Scope);

impl InstanceScope {
    /// Create a retained host instance. The host must eventually close, drain,
    /// release resources, and finish it; dropping observers alone never unloads.
    #[must_use]
    pub fn new(runtime: PluginRuntimeScopeId) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            Key::Instance,
            Node::new(
                None,
                LifecycleState::Created,
                PluginConcurrencyConfig::Concurrent,
            ),
        );
        let tree = Arc::new(Tree {
            runtime,
            instance: PluginInstanceId::fresh(),
            nodes: Mutex::new(nodes),
            changed: watch::channel(()).0,
            retained: Mutex::new(None),
        });
        // Fresh mutex cannot be poisoned. Explicit lifecycle finish breaks this
        // cycle only after every owned child, task and resource was released.
        *tree.retained.lock().expect("fresh instance retention") = Some(tree.clone());
        Self(Scope {
            tree,
            key: Key::Instance,
        })
    }

    #[must_use]
    pub fn id(&self) -> PluginInstanceId {
        self.0.tree.instance
    }

    /// The concurrency policy bounds simultaneously retained invocations, not
    /// nested callbacks. Nested callback admission is deliberately reentrant.
    pub fn begin_activation(
        &self,
        policy: PluginConcurrencyConfig,
    ) -> Result<ActivationScope, ScopeError> {
        let mut nodes = self.0.tree.lock()?;
        let parent = nodes.get_mut(&Key::Instance).ok_or(ScopeError::Closed)?;
        if !matches!(
            parent.phase,
            LifecycleState::Created | LifecycleState::Inactive
        ) {
            return Err(ScopeError::InvalidTransition);
        }
        parent.phase = LifecycleState::Activating;
        let key = Key::Activation(PluginActivationId::fresh());
        nodes.insert(
            key,
            Node::new(Some(Key::Instance), LifecycleState::Activating, policy),
        );
        Ok(ActivationScope(Scope {
            tree: self.0.tree.clone(),
            key,
        }))
    }
}

impl ActivationScope {
    #[must_use]
    pub fn id(&self) -> PluginActivationId {
        let Key::Activation(id) = self.0.key else {
            unreachable!("activation key")
        };
        id
    }

    /// Activation completion is not a readiness claim; readiness is host policy.
    pub fn mark_active(&self) -> Result<(), ScopeError> {
        let mut nodes = self.0.tree.lock()?;
        if nodes
            .get(&Key::Instance)
            .is_none_or(|node| node.phase != LifecycleState::Activating)
            || nodes
                .get(&self.0.key)
                .is_none_or(|node| node.phase != LifecycleState::Activating)
        {
            return Err(ScopeError::InvalidTransition);
        }
        if nodes
            .values()
            .any(|node| node.parent == Some(self.0.key) && node.lifecycle_authority.is_some())
        {
            return Err(ScopeError::Outstanding);
        }
        nodes
            .get_mut(&Key::Instance)
            .ok_or(ScopeError::Closed)?
            .phase = LifecycleState::Active;
        nodes.get_mut(&self.0.key).ok_or(ScopeError::Closed)?.phase = LifecycleState::Active;
        Ok(())
    }

    /// Mint callback authority for one trusted host lifecycle operation, never
    /// for ordinary service input. Only one lifecycle root may be retained at a
    /// time; retire it explicitly before starting the next lifecycle operation.
    /// Cleanup requires zero admitted activation work but does not clear retained
    /// failures. Its fresh cancellation channel is not the already-cancelled
    /// activation channel: later explicit close still cancels the entire subtree.
    pub fn begin_lifecycle_invocation(
        &self,
        kind: LifecycleInvocationKind,
    ) -> Result<InvocationScope, ScopeError> {
        let mut nodes = self.0.tree.lock()?;
        let activation = nodes.get(&self.0.key).ok_or(ScopeError::Closed)?;
        let instance = nodes.get(&Key::Instance).ok_or(ScopeError::Closed)?;
        let permitted = match kind {
            LifecycleInvocationKind::Activation => {
                activation.phase == LifecycleState::Activating
                    && instance.phase == LifecycleState::Activating
            }
            LifecycleInvocationKind::Cleanup => {
                matches!(
                    activation.phase,
                    LifecycleState::Closing | LifecycleState::CleanupRequired
                ) && matches!(
                    instance.phase,
                    LifecycleState::Activating
                        | LifecycleState::Active
                        | LifecycleState::Closing
                        | LifecycleState::CleanupRequired
                )
            }
        };
        if !permitted {
            return Err(ScopeError::InvalidTransition);
        }
        if kind == LifecycleInvocationKind::Cleanup && activation.admitted != 0 {
            return Err(ScopeError::Outstanding);
        }
        if nodes
            .values()
            .any(|node| node.parent == Some(self.0.key) && node.lifecycle_authority.is_some())
        {
            return Err(ScopeError::Busy);
        }
        if nodes.len() >= 1026 {
            return Err(ScopeError::Capacity);
        }
        let key = Key::Invocation(RuntimeInvocationId::fresh());
        let mut node = Node::new(
            Some(self.0.key),
            LifecycleState::Active,
            PluginConcurrencyConfig::Concurrent,
        );
        node.lifecycle_authority = Some(kind);
        nodes.insert(key, node);
        Ok(InvocationScope(Scope {
            tree: self.0.tree.clone(),
            key,
        }))
    }

    pub fn begin_invocation(&self) -> Result<InvocationScope, ScopeError> {
        let mut nodes = self.0.tree.lock()?;
        for key in ancestry(&nodes, self.0.key)? {
            if nodes[&key].phase != LifecycleState::Active {
                return Err(ScopeError::Closed);
            }
        }
        let count = nodes
            .values()
            .filter(|node| node.parent == Some(self.0.key))
            .count();
        if count >= limit(nodes[&self.0.key].policy) {
            return Err(ScopeError::Busy);
        }
        if count >= 1024 || nodes.len() >= 1026 {
            return Err(ScopeError::Capacity);
        }
        let key = Key::Invocation(RuntimeInvocationId::fresh());
        nodes.insert(
            key,
            Node::new(
                Some(self.0.key),
                LifecycleState::Active,
                PluginConcurrencyConfig::Concurrent,
            ),
        );
        Ok(InvocationScope(Scope {
            tree: self.0.tree.clone(),
            key,
        }))
    }
}

impl InvocationScope {
    /// A nested operation owns a distinct identity without reacquiring the
    /// activation's exclusive top-level invocation slot. Closing either ancestor
    /// fences this child. Parent finish requires explicit child retirement.
    pub fn begin_child(&self) -> Result<Self, ScopeError> {
        let mut nodes = self.0.tree.lock()?;
        let ancestors = ancestry(&nodes, self.0.key)?;
        if ancestors.len() >= 32 || nodes.len() >= 1026 {
            return Err(ScopeError::Capacity);
        }
        if !accepts_path(&nodes, &ancestors) {
            return Err(ScopeError::Closed);
        }
        let key = Key::Invocation(RuntimeInvocationId::fresh());
        nodes.insert(
            key,
            Node::new(
                Some(self.0.key),
                LifecycleState::Active,
                PluginConcurrencyConfig::Concurrent,
            ),
        );
        Ok(Self(Scope {
            tree: self.0.tree.clone(),
            key,
        }))
    }

    pub fn activation_id(&self) -> Result<PluginActivationId, ScopeError> {
        let nodes = self.0.tree.lock()?;
        match ancestry(&nodes, self.0.key)?.into_iter().find_map(|key| {
            if let Key::Activation(id) = key {
                Some(id)
            } else {
                None
            }
        }) {
            Some(id) => Ok(id),
            None => Err(ScopeError::InvalidTransition),
        }
    }

    #[must_use]
    pub fn id(&self) -> RuntimeInvocationId {
        let Key::Invocation(id) = self.0.key else {
            unreachable!("invocation key")
        };
        id
    }

    /// Create an owned callback capability, never a raw state pointer. The
    /// capability retains its state even if its originating caller disappears.
    pub fn callback<T: Any + Send + Sync>(
        &self,
        state: Arc<T>,
    ) -> Result<CallbackCapability<T>, ScopeError> {
        let resource = self.0.register(state)?;
        Ok(CallbackCapability {
            scope: self.clone(),
            resource,
            marker: std::marker::PhantomData,
        })
    }
}

/// Host-side capability. Adapters must retain it until explicit retirement;
/// serializing its resource ID alone never grants callback access.
pub struct CallbackCapability<T> {
    scope: InvocationScope,
    resource: PluginResourceId,
    marker: std::marker::PhantomData<fn() -> T>,
}

impl<T: Any + Send + Sync> CallbackCapability<T> {
    #[must_use]
    pub fn id(&self) -> PluginResourceId {
        self.resource
    }

    pub fn try_admit(&self) -> Result<CallbackAdmission<T>, ScopeError> {
        let guard = self.scope.try_admit()?;
        let nodes = self.scope.0.tree.lock()?;
        let state = nodes
            .get(&self.scope.0.key)
            .and_then(|node| node.resources.get(&self.resource))
            .ok_or(ScopeError::UnknownResource)?
            .clone()
            .downcast::<T>()
            .map_err(|_| ScopeError::UnknownResource)?;
        drop(nodes);
        Ok(CallbackAdmission {
            _guard: guard,
            state,
        })
    }
}

/// Shared callback access. Interior mutability, if required, belongs to callback
/// implementation; do not hold an exclusive lock while awaiting nested calls.
pub struct CallbackAdmission<T> {
    // Drop plugin-owned state before releasing the final lifetime admission.
    state: Arc<T>,
    _guard: AdmissionGuard,
}

impl<T> std::ops::Deref for CallbackAdmission<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.state
    }
}

macro_rules! scope_api {
    ($scope:ident) => {
        impl $scope {
            #[must_use]
            pub fn runtime_id(&self) -> PluginRuntimeScopeId {
                self.0.tree.runtime
            }
            #[must_use]
            pub fn instance_id(&self) -> PluginInstanceId {
                self.0.tree.instance
            }
            pub fn state(&self) -> Result<LifecycleState, ScopeError> {
                self.0.phase()
            }
            pub fn try_admit(&self) -> Result<AdmissionGuard, ScopeError> {
                self.0.admit()
            }
            /// Close this subtree and cancel its producers. Does not drain or unload.
            pub fn close(&self) -> Result<(), ScopeError> {
                self.0.close()
            }
            /// Cancellation-safe observation; dropping this future retains owners.
            pub async fn drain(&self) -> Result<(), ScopeError> {
                self.0.drain().await
            }
            /// Finish only after all children, tasks, admissions and resources settle.
            pub fn finish(&self) -> Result<(), ScopeError> {
                self.0.finish()
            }
            pub fn fail_cleanup(&self, error: String) -> Result<(), ScopeError> {
                self.0.fail_cleanup(error)
            }
            /// Host acknowledgement after repairing failed cleanup. Requires all
            /// retained work and resources to have settled before clearing failure.
            pub fn resolve_cleanup(&self) -> Result<(), ScopeError> {
                self.0.resolve_cleanup()
            }
            /// Register an owner before publication. The tree retains it until explicit release.
            pub fn register_resource<T: Any + Send + Sync>(
                &self,
                value: Arc<T>,
            ) -> Result<PluginResourceId, ScopeError> {
                self.0.register(value)
            }
            /// Confirm external cleanup before release. Rejected while work is admitted.
            /// Drop the returned owner outside all loader/runtime locks.
            pub fn release_resource(
                &self,
                id: PluginResourceId,
            ) -> Result<Arc<dyn Any + Send + Sync>, ScopeError> {
                self.0.release(id)
            }
            pub fn spawn<F, Fut>(&self, name: &str, work: F) -> Result<(), ScopeError>
            where
                F: FnOnce(TaskCancellation) -> Fut + Send + 'static,
                Fut: Future<Output = Result<(), String>> + Send + 'static,
            {
                self.0.spawn(name, work)
            }
            /// Live nonblocking cancellation observation for synchronous adapters.
            pub fn is_cancelled(&self) -> Result<bool, ScopeError> {
                let nodes = self.0.tree.lock()?;
                let node = nodes.get(&self.0.key).ok_or(ScopeError::Closed)?;
                Ok(*node.cancellation.borrow())
            }
            pub async fn cancelled(&self) -> Result<(), ScopeError> {
                let mut cancellation = self
                    .0
                    .tree
                    .lock()?
                    .get(&self.0.key)
                    .ok_or(ScopeError::Closed)?
                    .cancellation
                    .subscribe();
                loop {
                    if *cancellation.borrow_and_update() {
                        return Ok(());
                    }
                    cancellation
                        .changed()
                        .await
                        .map_err(|_| ScopeError::Closed)?;
                }
            }
        }
    };
}

scope_api!(InstanceScope);
scope_api!(ActivationScope);
scope_api!(InvocationScope);

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
