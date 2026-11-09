//! Instance, activation and invocation ownership with atomic admission fencing.
//!
//! A tree has one short-held metadata lock. No plugin code runs under it. Closing
//! and admission linearize under that lock; an admission increments every ancestor
//! before releasing it. Drain uses a watch revision subscribed before inspecting
//! the predicate, so completion between inspection and suspension cannot be lost.
//!
//! Every admission, child scope and resource is attributable (creating call
//! site plus optional label). Attribution bookkeeping is kept inside the same
//! tree lock as the counts it describes, so it introduces no new lock ordering.
//! Host teardown uses deadline-bounded drains that report stragglers by owner
//! instead of waiting forever.
use std::{
    any::Any,
    collections::BTreeMap,
    future::Future,
    panic::Location,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;

mod holders;
pub use holders::{HolderKind, HolderSite, OutstandingHolder, OutstandingHolders, ScopeKey};

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
    #[error("scope has outstanding admissions, children, tasks, or resources: {0}")]
    Outstanding(Box<OutstandingHolders>),
    #[error("scope teardown deadline exceeded; abandoning retained owners: {0}")]
    DeadlineExceeded(Box<OutstandingHolders>),
    #[error("scope state lock poisoned; cleanup authority retained")]
    Poisoned,
    #[error("scope cleanup failed: {0}")]
    Cleanup(String),
    #[error("resource does not belong to this scope")]
    UnknownResource,
}

type Key = ScopeKey;

/// Default bound for host teardown drains and process shutdown waits.
pub const DEFAULT_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Environment override (whole seconds) for [`teardown_timeout`].
pub const TEARDOWN_TIMEOUT_ENV: &str = "BMUX_TEARDOWN_TIMEOUT_SECS";
/// Grace after aborting remaining tasks at a teardown deadline.
const ABORT_GRACE: Duration = Duration::from_millis(250);

/// Host teardown bound: [`TEARDOWN_TIMEOUT_ENV`] seconds when set to a positive
/// integer, otherwise [`DEFAULT_TEARDOWN_TIMEOUT`].
#[must_use]
pub fn teardown_timeout() -> Duration {
    std::env::var(TEARDOWN_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map_or(DEFAULT_TEARDOWN_TIMEOUT, Duration::from_secs)
}

/// Deadline `teardown_timeout()` from now on the tokio clock.
#[must_use]
pub fn teardown_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + teardown_timeout()
}

/// Attribution of one live admission.
#[derive(Debug, Clone)]
struct Holder {
    site: HolderSite,
    label: Option<Arc<str>>,
}

struct ResourceEntry {
    value: Arc<dyn Any + Send + Sync>,
    site: HolderSite,
    type_name: &'static str,
}

struct Node {
    parent: Option<Key>,
    site: HolderSite,
    label: Option<Arc<str>>,
    /// Live admissions on this node, bounded by `direct` (at most 1024).
    holders: Vec<(u64, Holder)>,
    phase: LifecycleState,
    admitted: usize,
    direct: usize,
    policy: PluginConcurrencyConfig,
    cancellation: watch::Sender<bool>,
    tasks: BackgroundTasks,
    tasks_drained: bool,
    resources: BTreeMap<PluginResourceId, ResourceEntry>,
    failure: Option<String>,
    lifecycle_authority: Option<LifecycleInvocationKind>,
}

impl Node {
    fn new(
        parent: Option<Key>,
        phase: LifecycleState,
        policy: PluginConcurrencyConfig,
        site: HolderSite,
    ) -> Self {
        Self {
            parent,
            site,
            label: None,
            holders: Vec::new(),
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
    next_holder: AtomicU64,
    changed: watch::Sender<()>,
    // Deliberate retention until explicit successful instance finish. Losing all
    // observers must leak safely rather than unload code needed by detached work.
    retained: Mutex<Option<Arc<Tree>>>,
}

impl Tree {
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<Key, Node>>, ScopeError> {
        self.nodes.lock().map_err(|_| ScopeError::Poisoned)
    }

    /// Snapshot the holders of `root`'s subtree. Runs under the tree lock the
    /// caller already holds; never takes task locks.
    fn report(&self, nodes: &BTreeMap<Key, Node>, root: Key) -> OutstandingHolders {
        let mut holders = Vec::new();
        for key in subtree(nodes, root) {
            let node = &nodes[&key];
            if key != root {
                holders.push(OutstandingHolder {
                    kind: HolderKind::ChildScope,
                    scope: key,
                    scope_label: node.label.clone(),
                    site: Some(node.site),
                    label: None,
                });
            }
            holders.extend(node.holders.iter().map(|(_, holder)| OutstandingHolder {
                kind: HolderKind::Admission,
                scope: key,
                scope_label: node.label.clone(),
                site: Some(holder.site),
                label: holder.label.clone(),
            }));
            holders.extend(node.resources.values().map(|entry| OutstandingHolder {
                kind: HolderKind::Resource,
                scope: key,
                scope_label: node.label.clone(),
                site: Some(entry.site),
                label: Some(Arc::from(entry.type_name)),
            }));
            if !node.tasks_drained {
                holders.push(OutstandingHolder {
                    kind: HolderKind::UndrainedTasks,
                    scope: key,
                    scope_label: node.label.clone(),
                    site: None,
                    label: None,
                });
            }
        }
        OutstandingHolders {
            runtime: self.runtime,
            instance: self.instance,
            instance_label: nodes
                .get(&Key::Instance)
                .and_then(|node| node.label.clone()),
            scope: root,
            phase: nodes
                .get(&root)
                .map_or(LifecycleState::Destroyed, |node| node.phase),
            holders,
        }
    }

    fn outstanding(&self, nodes: &BTreeMap<Key, Node>, root: Key) -> ScopeError {
        ScopeError::Outstanding(Box::new(self.report(nodes, root)))
    }
}

/// Keys of `root` and all of its descendants, root first.
fn subtree(nodes: &BTreeMap<Key, Node>, root: Key) -> Vec<Key> {
    let mut keys = vec![root];
    keys.extend(
        nodes
            .keys()
            .copied()
            .filter(|key| *key != root)
            .filter(|key| ancestry(nodes, *key).is_ok_and(|path| path.contains(&root))),
    );
    keys
}

/// Weak registry of every instance tree, for process-shutdown diagnostics.
/// Only instance creation and reporting take this lock, never while holding a
/// tree lock.
fn live_trees() -> &'static Mutex<Vec<Weak<Tree>>> {
    static LIVE: OnceLock<Mutex<Vec<Weak<Tree>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Every unfinished instance in this process with its outstanding holders.
/// Used to name what blocks process shutdown; it never releases anything.
#[must_use]
pub fn unfinished_instances() -> Vec<OutstandingHolders> {
    let trees: Vec<Arc<Tree>> = live_trees()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    trees
        .into_iter()
        .filter_map(|tree| {
            let scope = Scope {
                tree,
                key: Key::Instance,
            };
            scope
                .outstanding()
                .ok()
                .filter(|report| report.phase != LifecycleState::Destroyed)
        })
        .collect()
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

    fn admit(
        &self,
        label: Option<Arc<str>>,
        site: HolderSite,
    ) -> Result<AdmissionGuard, ScopeError> {
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
        let holder = self.tree.next_holder.fetch_add(1, Ordering::Relaxed);
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
        node.direct += 1;
        node.holders.push((holder, Holder { site, label }));
        Ok(AdmissionGuard {
            scope: self.clone(),
            holder,
        })
    }

    fn set_label(&self, label: Arc<str>) -> Result<(), ScopeError> {
        self.tree
            .lock()?
            .get_mut(&self.key)
            .ok_or(ScopeError::Closed)?
            .label = Some(label);
        Ok(())
    }

    /// Snapshot this subtree's holders, including unreaped task names. Task
    /// locks are only taken after the tree lock is released.
    fn outstanding(&self) -> Result<OutstandingHolders, ScopeError> {
        let (mut report, tasks) = {
            let nodes = self.tree.lock()?;
            if !nodes.contains_key(&self.key) {
                return Err(ScopeError::Closed);
            }
            let tasks: Vec<_> = subtree(&nodes, self.key)
                .into_iter()
                .map(|key| (key, nodes[&key].label.clone(), nodes[&key].tasks.clone()))
                .collect();
            (self.tree.report(&nodes, self.key), tasks)
        };
        for (key, scope_label, tasks) in tasks {
            for name in tasks.pending_names() {
                report.holders.push(OutstandingHolder {
                    kind: HolderKind::Task,
                    scope: key,
                    scope_label: scope_label.clone(),
                    site: None,
                    label: Some(Arc::from(name)),
                });
            }
        }
        Ok(report)
    }

    /// Deadline expiry: report who blocked, then abandon. Retained owners are
    /// never released here; remaining tasks are aborted best-effort and the
    /// scope is marked as failed cleanup so it can never report success.
    async fn expire(&self) -> ScopeError {
        let report = match self.outstanding() {
            Ok(report) => report,
            Err(error) => return error,
        };
        let tasks = {
            let mut nodes = match self.tree.lock() {
                Ok(nodes) => nodes,
                Err(error) => return error,
            };
            let tasks: Vec<_> = subtree(&nodes, self.key)
                .into_iter()
                .map(|key| nodes[&key].tasks.clone())
                .collect();
            if let Some(node) = nodes.get_mut(&self.key) {
                node.phase = LifecycleState::CleanupRequired;
                node.failure = Some(format!("teardown deadline exceeded: {report}"));
            }
            tasks
        };
        self.tree.changed.send_replace(());
        // Never nest task and tree locks (see close).
        for task_scope in &tasks {
            task_scope.abort();
        }
        // Give aborted futures a moment to drop their admissions so the host
        // retains as little as possible; the result never becomes success.
        let mut changed = self.tree.changed.subscribe();
        let _ = tokio::time::timeout(ABORT_GRACE, async {
            loop {
                let settled = self
                    .tree
                    .lock()
                    .ok()
                    .and_then(|nodes| nodes.get(&self.key).map(|node| node.admitted == 0))
                    .unwrap_or(true);
                if settled || changed.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        tracing::warn!(
            report = %report,
            "plugin scope teardown deadline exceeded; owners abandoned (code is never unloaded)"
        );
        ScopeError::DeadlineExceeded(Box::new(report))
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

    async fn drain(&self, deadline: Option<tokio::time::Instant>) -> Result<(), ScopeError> {
        self.close()?;
        let tasks = self
            .tree
            .lock()?
            .get(&self.key)
            .ok_or(ScopeError::Closed)?
            .tasks
            .clone();
        let outcome = match deadline {
            None => tasks.shutdown_retained(None).await,
            Some(deadline) => {
                if let Ok(outcome) =
                    tokio::time::timeout_at(deadline, tasks.shutdown_retained(None)).await
                {
                    outcome
                } else {
                    return Err(self.expire().await);
                }
            }
        };
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
            match deadline {
                None => changed.changed().await.map_err(|_| ScopeError::Closed)?,
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, changed.changed()).await {
                        Ok(changed) => changed.map_err(|_| ScopeError::Closed)?,
                        Err(_) => return Err(self.expire().await),
                    }
                }
            }
        }
    }

    fn spawn<F, Fut>(&self, name: &str, work: F, site: HolderSite) -> Result<(), ScopeError>
    where
        F: FnOnce(TaskCancellation) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let admission = self.admit(Some(Arc::from(name)), site)?;
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
        site: HolderSite,
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
        node.resources.insert(
            id,
            ResourceEntry {
                value: resource,
                site,
                type_name: std::any::type_name::<T>(),
            },
        );
        Ok(id)
    }

    fn release(
        &self,
        resource: PluginResourceId,
    ) -> Result<Arc<dyn Any + Send + Sync>, ScopeError> {
        let mut nodes = self.tree.lock()?;
        if nodes.values().any(|child| child.parent == Some(self.key))
            || nodes.get(&self.key).ok_or(ScopeError::Closed)?.admitted != 0
        {
            return Err(self.tree.outstanding(&nodes, self.key));
        }
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
        // Return ownership, so a plugin-owned destructor never runs under lock.
        node.resources
            .remove(&resource)
            .map(|entry| entry.value)
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
            return Err(self.tree.outstanding(&nodes, self.key));
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
            return Err(self.tree.outstanding(&nodes, self.key));
        }
        let node = nodes.get(&self.key).ok_or(ScopeError::Closed)?;
        if node.phase != LifecycleState::CleanupRequired {
            return Err(ScopeError::InvalidTransition);
        }
        if node.admitted != 0 || !node.tasks_drained || !node.resources.is_empty() {
            return Err(self.tree.outstanding(&nodes, self.key));
        }
        let node = nodes.get_mut(&self.key).ok_or(ScopeError::Closed)?;
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
    holder: u64,
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
                // Capacity is retained, so steady-state admission allocates nothing.
                if let Some(index) = node.holders.iter().position(|(id, _)| *id == self.holder) {
                    node.holders.swap_remove(index);
                }
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
    #[track_caller]
    pub fn new(runtime: PluginRuntimeScopeId) -> Self {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            Key::Instance,
            Node::new(
                None,
                LifecycleState::Created,
                PluginConcurrencyConfig::Concurrent,
                Location::caller(),
            ),
        );
        let tree = Arc::new(Tree {
            runtime,
            instance: PluginInstanceId::fresh(),
            nodes: Mutex::new(nodes),
            changed: watch::channel(()).0,
            retained: Mutex::new(None),
            next_holder: AtomicU64::new(0),
        });
        // Fresh mutex cannot be poisoned. Explicit lifecycle finish breaks this
        // cycle only after every owned child, task and resource was released.
        *tree.retained.lock().expect("fresh instance retention") = Some(tree.clone());
        {
            let mut live = live_trees()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            live.retain(|tree| tree.strong_count() > 0);
            live.push(Arc::downgrade(&tree));
        }
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
    #[track_caller]
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
            Node::new(
                Some(Key::Instance),
                LifecycleState::Activating,
                policy,
                Location::caller(),
            ),
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
            return Err(self.0.tree.outstanding(&nodes, self.0.key));
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
    #[track_caller]
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
            return Err(self.0.tree.outstanding(&nodes, self.0.key));
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
            Location::caller(),
        );
        node.lifecycle_authority = Some(kind);
        nodes.insert(key, node);
        Ok(InvocationScope(Scope {
            tree: self.0.tree.clone(),
            key,
        }))
    }

    #[track_caller]
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
                Location::caller(),
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
    #[track_caller]
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
                Location::caller(),
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
    #[track_caller]
    pub fn callback<T: Any + Send + Sync>(
        &self,
        state: Arc<T>,
    ) -> Result<CallbackCapability<T>, ScopeError> {
        let resource = self.0.register(state, Location::caller())?;
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

    #[track_caller]
    pub fn try_admit(&self) -> Result<CallbackAdmission<T>, ScopeError> {
        let guard = self.scope.try_admit()?;
        let nodes = self.scope.0.tree.lock()?;
        let state = nodes
            .get(&self.scope.0.key)
            .and_then(|node| node.resources.get(&self.resource))
            .ok_or(ScopeError::UnknownResource)?
            .value
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
            /// Admit one operation, attributed to the caller's source location.
            #[track_caller]
            pub fn try_admit(&self) -> Result<AdmissionGuard, ScopeError> {
                self.0.admit(None, Location::caller())
            }
            /// Admit one operation with a diagnostic label (e.g. an operation
            /// name) in addition to the caller's source location.
            #[track_caller]
            pub fn try_admit_labeled(
                &self,
                label: impl Into<Arc<str>>,
            ) -> Result<AdmissionGuard, ScopeError> {
                self.0.admit(Some(label.into()), Location::caller())
            }
            /// Attach a diagnostic label (e.g. plugin id or operation) to this
            /// scope; it is reported with every holder the scope owns.
            pub fn set_label(&self, label: impl Into<Arc<str>>) -> Result<(), ScopeError> {
                self.0.set_label(label.into())
            }
            /// Every outstanding admission, child scope, resource and task of
            /// this subtree, attributed by creating site and label.
            pub fn outstanding(&self) -> Result<OutstandingHolders, ScopeError> {
                self.0.outstanding()
            }
            /// Close this subtree and cancel its producers. Does not drain or unload.
            pub fn close(&self) -> Result<(), ScopeError> {
                self.0.close()
            }
            /// Cancellation-safe observation; dropping this future retains owners.
            /// Waits without bound; host teardown must use [`Self::drain_until`].
            pub async fn drain(&self) -> Result<(), ScopeError> {
                self.0.drain(None).await
            }
            /// Close and drain until `deadline`. On expiry, remaining tasks are
            /// aborted best-effort, the scope is marked failed cleanup, and
            /// [`ScopeError::DeadlineExceeded`] names the outstanding holders.
            /// Owners are abandoned (retained), never released or unloaded.
            pub async fn drain_until(
                &self,
                deadline: tokio::time::Instant,
            ) -> Result<(), ScopeError> {
                self.0.drain(Some(deadline)).await
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
            #[track_caller]
            pub fn register_resource<T: Any + Send + Sync>(
                &self,
                value: Arc<T>,
            ) -> Result<PluginResourceId, ScopeError> {
                self.0.register(value, Location::caller())
            }
            /// Confirm external cleanup before release. Rejected while work is admitted.
            /// Drop the returned owner outside all loader/runtime locks.
            pub fn release_resource(
                &self,
                id: PluginResourceId,
            ) -> Result<Arc<dyn Any + Send + Sync>, ScopeError> {
                self.0.release(id)
            }
            #[track_caller]
            pub fn spawn<F, Fut>(&self, name: &str, work: F) -> Result<(), ScopeError>
            where
                F: FnOnce(TaskCancellation) -> Fut + Send + 'static,
                Fut: Future<Output = Result<(), String>> + Send + 'static,
            {
                self.0.spawn(name, work, Location::caller())
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
mod holder_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod tests;
