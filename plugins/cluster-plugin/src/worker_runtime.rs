//! Worker-side fencing and bounded execution-output primitives.
//!
//! Concrete pane-runtime orchestration owns local PTY bindings. These helpers
//! enforce the cluster execution authority and cursor invariants independently
//! of that backend so retries and restart reconciliation use one implementation.

use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ExecutionId, WorkerAuthority, WorkerExecution, WorkerExecutionList,
    WorkerExecutionState, WorkerLaunchResult, WorkerOperationClass, WorkerQueryResult,
    WorkerRegistryStats, WorkerServiceError,
};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Instant;

pub const MAX_CONTROL_LEASE_DURATION_MS: u64 = 5_000;
/// How long after its issue time (beyond its duration) an observe lease is still accepted, for
/// clock differences between the issuing node and this one.
const OBSERVE_CLOCK_SKEW_MS: u64 = 30_000;

pub trait WorkerPaneRuntime: Send + Sync {
    /// Starts a local runtime with the requested command, environment, and PTY
    /// dimensions for one committed execution.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when local runtime creation fails.
    fn launch(
        &self,
        authority: &WorkerAuthority,
        spec: &bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError>;

    /// Adopts an existing local pane without restarting its process.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when the local binding is absent.
    fn adopt(
        &self,
        authority: &WorkerAuthority,
        spec: &bmux_cluster_plugin_api::cluster_types::WorkerAdoptionSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError>;

    /// Writes input to the exact bound local pane.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when the local runtime rejects input.
    fn input(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        data: &[u8],
    ) -> Result<(), WorkerServiceError>;

    /// Pushes frames of `targets` (local session, pane) to the connection `client_id`, which
    /// observes them; an empty set stops it.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when the local runtime refuses the observer.
    fn observe(
        &self,
        client_id: uuid::Uuid,
        targets: &[(uuid::Uuid, uuid::Uuid)],
        max_history_lines: u32,
    ) -> Result<(), WorkerServiceError>;

    /// Applies the viewport size to the exact bound local pane/session.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when resize fails.
    fn resize(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<(), WorkerServiceError>;

    /// Signals the exact bound local pane process.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when signal delivery fails.
    fn signal(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        signal: bmux_cluster_plugin_api::cluster_types::WorkerSignal,
    ) -> Result<(), WorkerServiceError>;

    /// Restarts the exact local pane using the supplied launch specification.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when restart fails.
    fn restart(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        spec: &bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError>;

    /// Closes the exact local pane binding.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when close fails.
    fn close(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> Result<(), WorkerServiceError>;

    /// Reports whether a local runtime binding still exists.
    fn contains(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> bool;
}

pub struct WorkerRegistry<R, V> {
    local_node_id: String,
    runtime: R,
    verifier: V,
    inner: Mutex<WorkerRegistryInner>,
}

struct WorkerRegistryInner {
    executions: BTreeMap<uuid::Uuid, WorkerRegistryEntry>,
    fence: WorkerFenceState,
    command_outcomes: WorkerCommandDedup<RegistryOutcome>,
}

struct WorkerRegistryEntry {
    execution: WorkerExecution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryOutcome {
    Execution(Box<WorkerExecution>),
    Mutation(bmux_cluster_plugin_api::cluster_types::WorkerMutationAck),
}

impl<R, V> WorkerRegistry<R, V>
where
    R: WorkerPaneRuntime,
    V: WorkerLeaseVerifier,
{
    #[must_use]
    pub fn new(local_node_id: impl Into<String>, runtime: R, verifier: V) -> Self {
        Self {
            local_node_id: local_node_id.into(),
            runtime,
            verifier,
            inner: Mutex::new(WorkerRegistryInner {
                executions: BTreeMap::new(),
                fence: WorkerFenceState::default(),
                command_outcomes: WorkerCommandDedup::default(),
            }),
        }
    }

    /// Idempotently launches one committed execution and binds it to local
    /// pane-runtime identities.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, local runtime, or registry lock failures.
    pub fn launch(
        &self,
        command_id: &CommandId,
        authority: WorkerAuthority,
        spec: &bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
        monotonic_now_ms: u64,
    ) -> Result<WorkerExecution, WorkerServiceError> {
        let fingerprint = fingerprint(&(authority.clone(), spec))?;
        let mut inner = self.lock()?;
        if let Some(outcome) = existing_outcome(&inner.command_outcomes, command_id, &fingerprint)?
        {
            return outcome_execution(outcome);
        }
        if let Some(existing) = inner.executions.get(&authority.execution_id.value) {
            if existing.execution.authority == authority {
                return Ok(existing.execution.clone());
            }
            return Err(WorkerServiceError::CommandConflict);
        }
        ensure_new_execution(&inner.executions, &authority)?;
        let expected = expected_authority(&self.local_node_id, &authority);
        inner.fence.validate_mutation(
            &expected,
            &authority,
            WorkerOperationClass::Lifecycle,
            monotonic_now_ms,
            &self.verifier,
        )?;
        drop(inner);
        let (session_id, pane_id) = self.runtime.launch(&authority, spec)?;
        let execution = worker_execution(authority, session_id, pane_id);
        let mut inner = self.lock()?;
        let execution_id = execution.authority.execution_id.value;
        if let Some(existing) = inner.executions.get(&execution_id) {
            if existing.execution != execution {
                return Err(WorkerServiceError::CommandConflict);
            }
        } else {
            inner.executions.insert(
                execution_id,
                WorkerRegistryEntry {
                    execution: execution.clone(),
                },
            );
        }
        record_outcome(
            &mut inner.command_outcomes,
            command_id,
            fingerprint,
            RegistryOutcome::Execution(Box::new(execution.clone())),
        )?;
        drop(inner);
        Ok(execution)
    }

    /// Idempotently adopts an existing local pane without restarting it.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, missing-runtime, or registry lock failures.
    pub fn adopt(
        &self,
        command_id: &CommandId,
        authority: WorkerAuthority,
        spec: &bmux_cluster_plugin_api::cluster_types::WorkerAdoptionSpec,
        monotonic_now_ms: u64,
    ) -> Result<WorkerExecution, WorkerServiceError> {
        let fingerprint = fingerprint(&(authority.clone(), spec))?;
        let mut inner = self.lock()?;
        if let Some(outcome) = existing_outcome(&inner.command_outcomes, command_id, &fingerprint)?
        {
            return outcome_execution(outcome);
        }
        ensure_new_execution(&inner.executions, &authority)?;
        let expected = expected_authority(&self.local_node_id, &authority);
        inner.fence.validate_mutation(
            &expected,
            &authority,
            WorkerOperationClass::Lifecycle,
            monotonic_now_ms,
            &self.verifier,
        )?;
        drop(inner);
        let (session_id, pane_id) = self.runtime.adopt(&authority, spec)?;
        let execution = worker_execution(authority, session_id, pane_id);
        let execution_id = execution.authority.execution_id.value;
        let mut inner = self.lock()?;
        inner.executions.insert(
            execution_id,
            WorkerRegistryEntry {
                execution: execution.clone(),
            },
        );
        record_outcome(
            &mut inner.command_outcomes,
            command_id,
            fingerprint,
            RegistryOutcome::Execution(Box::new(execution.clone())),
        )?;
        drop(inner);
        Ok(execution)
    }

    /// Applies one generation-fenced input mutation.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, local runtime, or registry lock failures.
    pub fn input(
        &self,
        command_id: &CommandId,
        authority: &WorkerAuthority,
        data: &[u8],
        monotonic_now_ms: u64,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerMutationAck, WorkerServiceError> {
        self.mutate(
            command_id,
            authority,
            WorkerOperationClass::Interactive,
            &(authority, data),
            monotonic_now_ms,
            |execution| {
                self.runtime
                    .input(execution.local_session_id, execution.local_pane_id, data)
            },
        )
    }

    /// Applies one generation-fenced resize mutation.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, local runtime, or registry lock failures.
    pub fn resize(
        &self,
        command_id: &CommandId,
        authority: &WorkerAuthority,
        cols: u16,
        rows: u16,
        monotonic_now_ms: u64,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerMutationAck, WorkerServiceError> {
        self.mutate(
            command_id,
            authority,
            WorkerOperationClass::Interactive,
            &(authority, cols, rows),
            monotonic_now_ms,
            |execution| {
                self.runtime.resize(
                    execution.local_session_id,
                    execution.local_pane_id,
                    cols,
                    rows,
                )
            },
        )
    }

    /// Applies one generation-fenced signal mutation.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, local runtime, or registry lock failures.
    pub fn signal(
        &self,
        command_id: &CommandId,
        authority: &WorkerAuthority,
        signal: bmux_cluster_plugin_api::cluster_types::WorkerSignal,
        monotonic_now_ms: u64,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerMutationAck, WorkerServiceError> {
        self.mutate(
            command_id,
            authority,
            WorkerOperationClass::Lifecycle,
            &(authority, signal),
            monotonic_now_ms,
            |execution| {
                self.runtime
                    .signal(execution.local_session_id, execution.local_pane_id, signal)
            },
        )
    }

    /// Launches a committed replacement as a distinct execution identity and
    /// generation. The previous local binding is not re-used as global identity.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, local runtime, or registry lock failures.
    pub fn restart(
        &self,
        command_id: &CommandId,
        authority: WorkerAuthority,
        spec: &bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
        monotonic_now_ms: u64,
    ) -> Result<WorkerExecution, WorkerServiceError> {
        self.launch(command_id, authority, spec, monotonic_now_ms)
    }

    /// Applies one generation-fenced close and keeps a closed execution record
    /// for idempotency and reconciliation.
    ///
    /// # Errors
    ///
    /// Returns fencing, conflict, local runtime, or registry lock failures.
    pub fn close(
        &self,
        command_id: &CommandId,
        authority: &WorkerAuthority,
        monotonic_now_ms: u64,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerMutationAck, WorkerServiceError> {
        let ack = self.mutate(
            command_id,
            authority,
            WorkerOperationClass::Lifecycle,
            authority,
            monotonic_now_ms,
            |execution| {
                self.runtime
                    .close(execution.local_session_id, execution.local_pane_id)
            },
        )?;
        let mut inner = self.lock()?;
        if let Some(entry) = inner.executions.get_mut(&authority.execution_id.value) {
            entry.execution.state = WorkerExecutionState::Closed;
        }
        drop(inner);
        Ok(ack)
    }

    /// Returns one execution binding without exposing local IDs as global
    /// identity.
    ///
    /// # Errors
    ///
    /// Returns a registry lock failure.
    pub fn get(&self, execution_id: &ExecutionId) -> Result<WorkerQueryResult, WorkerServiceError> {
        let inner = self.lock()?;
        Ok(inner
            .executions
            .get(&execution_id.value)
            .map_or(WorkerQueryResult::Missing, |entry| {
                WorkerQueryResult::Found {
                    execution: entry.execution.clone(),
                }
            }))
    }

    /// Returns a canonical inventory ordered by execution UUID.
    ///
    /// # Errors
    ///
    /// Returns a registry lock failure.
    pub fn inventory(&self) -> Result<WorkerExecutionList, WorkerServiceError> {
        let inner = self.lock()?;
        Ok(WorkerExecutionList {
            executions: inner
                .executions
                .values()
                .map(|entry| entry.execution.clone())
                .collect(),
        })
    }

    /// Has the local pane runtime push frames of the executions `authorities` authorize to the
    /// connection `client_id` (replacing what it observed; none stops it), and returns where each
    /// is, so the observer can tell their frames apart.
    ///
    /// # Errors
    ///
    /// Returns missing execution, stale generation, invalid lease, local runtime, or lock
    /// failures; nothing is observed then.
    pub fn observe(
        &self,
        client_id: uuid::Uuid,
        authorities: &[WorkerAuthority],
        max_history_lines: u32,
        now_unix_ms: u64,
    ) -> Result<Vec<bmux_cluster_plugin_api::cluster_types::ObservedExecution>, WorkerServiceError>
    {
        let mut observed = Vec::with_capacity(authorities.len());
        let inner = self.lock()?;
        for authority in authorities {
            let entry = execution_entry(
                &inner.executions,
                &authority.execution_id,
                authority.generation,
            )?;
            let expected = expected_authority(&self.local_node_id, &entry.execution.authority);
            inner
                .fence
                .validate_observation(&expected, authority, now_unix_ms, &self.verifier)?;
            observed.push(bmux_cluster_plugin_api::cluster_types::ObservedExecution {
                execution_id: authority.execution_id.clone(),
                generation: authority.generation,
                session_id: entry.execution.local_session_id,
                pane_id: entry.execution.local_pane_id,
            });
        }
        // The local runtime is called without the registry held.
        drop(inner);
        let targets: Vec<(uuid::Uuid, uuid::Uuid)> = observed
            .iter()
            .map(|execution| (execution.session_id, execution.pane_id))
            .collect();
        self.runtime
            .observe(client_id, &targets, max_history_lines)?;
        Ok(observed)
    }

    /// Reconciles durable execution records against local pane-runtime state,
    /// quarantining stale/conflicting bindings and marking missing bindings
    /// unavailable without deleting them.
    ///
    /// # Errors
    ///
    /// Returns invalid authoritative inventory or registry lock failures.
    pub fn reconcile(
        &self,
        authoritative: Vec<WorkerExecution>,
    ) -> Result<WorkerRegistryStats, WorkerServiceError> {
        let mut seen = BTreeSet::new();
        let mut inner = self.lock()?;
        for execution in authoritative {
            if execution.authority.audience_node_id != self.local_node_id
                || !seen.insert(execution.authority.execution_id.value)
            {
                return Err(WorkerServiceError::AuthorityMismatch);
            }
            let exists = self
                .runtime
                .contains(execution.local_session_id, execution.local_pane_id);
            let mut execution = execution;
            execution.state = if exists {
                WorkerExecutionState::Ready
            } else {
                WorkerExecutionState::Unavailable
            };
            let entry = inner
                .executions
                .entry(execution.authority.execution_id.value)
                .or_insert_with(|| WorkerRegistryEntry {
                    execution: execution.clone(),
                });
            if entry.execution.authority == execution.authority {
                entry.execution = execution;
            } else {
                entry.execution.state = WorkerExecutionState::Quarantined;
            }
        }
        for (id, entry) in &mut inner.executions {
            if !seen.contains(id) {
                entry.execution.state = WorkerExecutionState::Quarantined;
            }
        }
        Ok(registry_stats(&inner.executions))
    }

    fn mutate<T: serde::Serialize>(
        &self,
        command_id: &CommandId,
        authority: &WorkerAuthority,
        class: WorkerOperationClass,
        request: &T,
        monotonic_now_ms: u64,
        effect: impl FnOnce(&WorkerExecution) -> Result<(), WorkerServiceError>,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerMutationAck, WorkerServiceError> {
        let fingerprint = fingerprint(request)?;
        let mut inner = self.lock()?;
        if let Some(outcome) = existing_outcome(&inner.command_outcomes, command_id, &fingerprint)?
        {
            return outcome_mutation(outcome);
        }
        let expected = {
            let entry = execution_entry(
                &inner.executions,
                &authority.execution_id,
                authority.generation,
            )?;
            expected_authority(&self.local_node_id, &entry.execution.authority)
        };
        inner.fence.validate_mutation(
            &expected,
            authority,
            class,
            monotonic_now_ms,
            &self.verifier,
        )?;
        let execution = execution_entry(
            &inner.executions,
            &authority.execution_id,
            authority.generation,
        )?
        .execution
        .clone();
        drop(inner);
        effect(&execution)?;
        let ack = bmux_cluster_plugin_api::cluster_types::WorkerMutationAck {
            execution_id: authority.execution_id.clone(),
            generation: authority.generation,
            applied: true,
        };
        let mut inner = self.lock()?;
        {
            record_outcome(
                &mut inner.command_outcomes,
                command_id,
                fingerprint,
                RegistryOutcome::Mutation(ack.clone()),
            )?;
        }
        drop(inner);
        Ok(ack)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, WorkerRegistryInner>, WorkerServiceError> {
        self.inner
            .lock()
            .map_err(|_| WorkerServiceError::Unavailable {
                reason: "worker registry lock is poisoned".to_string(),
            })
    }
}

fn expected_authority(
    local_node_id: &str,
    authority: &WorkerAuthority,
) -> WorkerExecutionAuthority {
    WorkerExecutionAuthority {
        cluster_id: authority.cluster_id.clone(),
        workspace_id: authority.workspace_id.value,
        pane_id: authority.pane_id.value,
        execution_id: authority.execution_id.clone(),
        generation: authority.generation,
        local_node_id: local_node_id.to_string(),
    }
}

fn ensure_new_execution(
    executions: &BTreeMap<uuid::Uuid, WorkerRegistryEntry>,
    authority: &WorkerAuthority,
) -> Result<(), WorkerServiceError> {
    if executions.contains_key(&authority.execution_id.value) {
        Err(WorkerServiceError::CommandConflict)
    } else {
        Ok(())
    }
}

const fn worker_execution(
    authority: WorkerAuthority,
    local_session_id: uuid::Uuid,
    local_pane_id: uuid::Uuid,
) -> WorkerExecution {
    WorkerExecution {
        authority,
        local_session_id,
        local_pane_id,
        state: WorkerExecutionState::Ready,
        exit_code: None,
    }
}

fn execution_entry<'a>(
    executions: &'a BTreeMap<uuid::Uuid, WorkerRegistryEntry>,
    execution_id: &ExecutionId,
    generation: u64,
) -> Result<&'a WorkerRegistryEntry, WorkerServiceError> {
    let entry =
        executions
            .get(&execution_id.value)
            .ok_or_else(|| WorkerServiceError::NotFound {
                execution_id: execution_id.clone(),
            })?;
    require_entry_generation(entry, generation)?;
    Ok(entry)
}

const fn require_entry_generation(
    entry: &WorkerRegistryEntry,
    generation: u64,
) -> Result<(), WorkerServiceError> {
    if entry.execution.authority.generation == generation {
        Ok(())
    } else {
        Err(WorkerServiceError::StaleGeneration {
            expected: entry.execution.authority.generation,
            received: generation,
        })
    }
}

fn fingerprint(value: &impl serde::Serialize) -> Result<Vec<u8>, WorkerServiceError> {
    let encoded = bmux_plugin_sdk::encode_service_message(value).map_err(|error| {
        WorkerServiceError::InvalidRequest {
            reason: format!("worker request encoding failed: {error}"),
        }
    })?;
    Ok(sha2::Sha256::digest(encoded).to_vec())
}

fn existing_outcome<T: Clone>(
    dedup: &WorkerCommandDedup<T>,
    command_id: &CommandId,
    fingerprint: &[u8],
) -> Result<Option<T>, WorkerServiceError> {
    dedup
        .outcomes
        .get(&command_id.value)
        .map_or(Ok(None), |(known, outcome)| {
            if known == fingerprint {
                Ok(Some(outcome.clone()))
            } else {
                Err(WorkerServiceError::CommandConflict)
            }
        })
}

fn record_outcome<T: Clone>(
    dedup: &mut WorkerCommandDedup<T>,
    command_id: &CommandId,
    fingerprint: Vec<u8>,
    outcome: T,
) -> Result<(), WorkerServiceError> {
    dedup
        .resolve(command_id, fingerprint, || outcome)
        .map(|_| ())
}

fn outcome_execution(outcome: RegistryOutcome) -> Result<WorkerExecution, WorkerServiceError> {
    match outcome {
        RegistryOutcome::Execution(execution) => Ok(*execution),
        RegistryOutcome::Mutation(_) => Err(WorkerServiceError::CommandConflict),
    }
}

fn outcome_mutation(
    outcome: RegistryOutcome,
) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerMutationAck, WorkerServiceError> {
    match outcome {
        RegistryOutcome::Mutation(ack) => Ok(ack),
        RegistryOutcome::Execution(_) => Err(WorkerServiceError::CommandConflict),
    }
}

fn registry_stats(executions: &BTreeMap<uuid::Uuid, WorkerRegistryEntry>) -> WorkerRegistryStats {
    WorkerRegistryStats {
        execution_count: executions.len().try_into().unwrap_or(u32::MAX),
    }
}

pub struct NodeSignatureLeaseVerifier<C> {
    caller: std::sync::Arc<C>,
    local_node_id: crate::membership::NodeId,
    nodes: crate::consensus_network::ConsensusNodeRegistry,
}

impl<C> NodeSignatureLeaseVerifier<C> {
    #[must_use]
    pub const fn new(
        caller: std::sync::Arc<C>,
        local_node_id: crate::membership::NodeId,
        nodes: crate::consensus_network::ConsensusNodeRegistry,
    ) -> Self {
        Self {
            caller,
            local_node_id,
            nodes,
        }
    }
}

impl<C> WorkerLeaseVerifier for NodeSignatureLeaseVerifier<C>
where
    C: crate::ClusterRuntimeOps + bmux_plugin::ServiceCaller + Send + Sync + 'static,
{
    fn verify(&self, authority: &WorkerAuthority, payload: &[u8]) -> Result<(), String> {
        let control = crate::consensus_network::ControlServiceHandle::new(
            self.caller.clone(),
            self.local_node_id,
            self.nodes.clone(),
        );
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            format!("worker lease verification requires the host runtime: {error}")
        })?;
        let members =
            tokio::task::block_in_place(|| runtime.block_on(control.authoritative_members()))
                .map_err(|error| {
                    format!("authoritative worker lease membership read failed: {error:?}")
                })?;
        let issuer = members
            .members
            .iter()
            .find(|member| member.node_id == authority.issuer_node_id)
            .ok_or_else(|| "worker lease issuer is not a cluster member".to_string())?;
        if issuer.state != bmux_cluster_plugin_api::cluster_types::ClusterMemberState::Active
            || issuer.capabilities.consensus_role
                != bmux_cluster_plugin_api::cluster_types::ClusterConsensusRole::Voter
        {
            return Err("worker lease issuer is not an active voter".to_string());
        }
        crate::membership::verify_node_signature(
            &authority.issuer_node_id,
            payload,
            &authority.lease_signature,
        )
    }
}

/// Generated worker-service provider around one private execution registry.
///
/// The registry owns all mutable worker execution state. This handle provides
/// process-monotonic lease timing and async trait adaptation.
const WORKER_REGISTRY_STORAGE_KEY: &str = "cluster.worker.registry.v1";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredWorkerRegistry {
    version: u16,
    executions: Vec<WorkerExecution>,
}

#[derive(Clone)]
pub struct DurableWorkerRegistry<R, V, C> {
    registry: std::sync::Arc<WorkerRegistry<R, V>>,
    caller: std::sync::Arc<C>,
}

impl<R, V, C> DurableWorkerRegistry<R, V, C>
where
    R: WorkerPaneRuntime,
    V: WorkerLeaseVerifier,
    C: crate::ClusterRuntimeOps,
{
    #[must_use]
    pub const fn new(
        registry: std::sync::Arc<WorkerRegistry<R, V>>,
        caller: std::sync::Arc<C>,
    ) -> Self {
        Self { registry, caller }
    }

    /// Restores the durable authoritative inventory and reconciles it against
    /// the local pane runtime without launching, adopting, or deleting a pane.
    ///
    /// # Errors
    ///
    /// Returns corrupt-state, storage, authority, or registry failures.
    pub fn restore_and_reconcile(&self) -> Result<WorkerRegistryStats, String> {
        let executions = self.load()?;
        self.registry
            .reconcile(executions)
            .map_err(|error| format!("worker registry reconciliation failed: {error:?}"))
    }

    /// Persists the canonical registry inventory after a successful mutation.
    ///
    /// # Errors
    ///
    /// Returns inventory, encoding, storage-key, or storage-write failures.
    pub fn persist(&self) -> Result<(), String> {
        let record = StoredWorkerRegistry {
            version: 1,
            executions: self
                .registry
                .inventory()
                .map_err(|error| format!("worker inventory failed: {error:?}"))?
                .executions,
        };
        let value = bmux_plugin_sdk::encode_service_message(&record)
            .map_err(|error| format!("worker registry encoding failed: {error}"))?;
        self.caller
            .storage_set(&bmux_plugin_sdk::StorageSetRequest::new(
                bmux_plugin_sdk::StorageKey::new(WORKER_REGISTRY_STORAGE_KEY)
                    .map_err(|error| format!("worker registry storage key failed: {error}"))?,
                value,
            ))
    }

    fn load(&self) -> Result<Vec<WorkerExecution>, String> {
        let response = self
            .caller
            .storage_get(&bmux_plugin_sdk::StorageGetRequest::new(
                bmux_plugin_sdk::StorageKey::new(WORKER_REGISTRY_STORAGE_KEY)
                    .map_err(|error| format!("worker registry storage key failed: {error}"))?,
            ))?;
        let Some(value) = response.value else {
            return Ok(Vec::new());
        };
        let record = bmux_plugin_sdk::decode_service_message::<StoredWorkerRegistry>(&value)
            .map_err(|error| format!("worker registry state is corrupt: {error}"))?;
        if record.version != 1 {
            return Err(format!(
                "unsupported worker registry version {}",
                record.version
            ));
        }
        Ok(record.executions)
    }
}

pub struct WorkerServiceHandle<R, V, C> {
    registry: std::sync::Arc<WorkerRegistry<R, V>>,
    durable: Option<DurableWorkerRegistry<R, V, C>>,
    started_at: Instant,
}

impl<R, V, C> WorkerServiceHandle<R, V, C>
where
    R: WorkerPaneRuntime,
    V: WorkerLeaseVerifier,
{
    #[must_use]
    pub fn new(registry: WorkerRegistry<R, V>) -> Self {
        Self {
            registry: std::sync::Arc::new(registry),
            durable: None,
            started_at: Instant::now(),
        }
    }

    #[must_use]
    pub fn from_durable(durable: DurableWorkerRegistry<R, V, C>) -> Self {
        Self {
            registry: durable.registry.clone(),
            durable: Some(durable),
            started_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn registry(&self) -> &std::sync::Arc<WorkerRegistry<R, V>> {
        &self.registry
    }

    fn persist_after<T>(
        &self,
        result: Result<T, WorkerServiceError>,
    ) -> Result<T, WorkerServiceError>
    where
        C: crate::ClusterRuntimeOps,
    {
        let value = result?;
        if let Some(durable) = &self.durable {
            durable
                .persist()
                .map_err(|reason| WorkerServiceError::LocalRuntime { reason })?;
        }
        Ok(value)
    }

    fn monotonic_now_ms(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

const fn launch_result(execution: WorkerExecution) -> WorkerLaunchResult {
    match execution.state {
        WorkerExecutionState::Launching => WorkerLaunchResult::Pending { execution },
        WorkerExecutionState::Ready
        | WorkerExecutionState::Exited
        | WorkerExecutionState::Unavailable
        | WorkerExecutionState::Quarantined
        | WorkerExecutionState::Closed => WorkerLaunchResult::Ready { execution },
    }
}

impl<R, V, C> bmux_cluster_plugin_api::cluster_worker_command::ClusterWorkerCommandService
    for WorkerServiceHandle<R, V, C>
where
    R: WorkerPaneRuntime + 'static,
    V: WorkerLeaseVerifier + 'static,
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn launch<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
        spec: bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerLaunchResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.persist_after(
                self.registry
                    .launch(&command_id, authority, &spec, self.monotonic_now_ms())
                    .map(launch_result),
            )
        })
    }

    fn adopt<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
        spec: bmux_cluster_plugin_api::cluster_types::WorkerAdoptionSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerLaunchResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.persist_after(
                self.registry
                    .adopt(&command_id, authority, &spec, self.monotonic_now_ms())
                    .map(launch_result),
            )
        })
    }

    fn input<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
        data: Vec<u8>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerMutationAck,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.registry
                .input(&command_id, &authority, &data, self.monotonic_now_ms())
        })
    }

    fn resize<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
        cols: u16,
        rows: u16,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerMutationAck,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.registry
                .resize(&command_id, &authority, cols, rows, self.monotonic_now_ms())
        })
    }

    fn signal<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
        signal: bmux_cluster_plugin_api::cluster_types::WorkerSignal,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerMutationAck,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.registry
                .signal(&command_id, &authority, signal, self.monotonic_now_ms())
        })
    }

    fn restart<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
        spec: bmux_cluster_plugin_api::cluster_types::WorkerLaunchSpec,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerLaunchResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.registry
                .restart(&command_id, authority, &spec, self.monotonic_now_ms())
                .map(launch_result)
        })
    }

    fn close<'a>(
        &'a self,
        command_id: CommandId,
        authority: WorkerAuthority,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerMutationAck,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.registry
                .close(&command_id, &authority, self.monotonic_now_ms())
        })
    }

    fn reconcile<'a>(
        &'a self,
        executions: Vec<WorkerExecution>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerRegistryStats, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.persist_after(self.registry.reconcile(executions)) })
    }
}

impl<R, V, C> bmux_cluster_plugin_api::cluster_worker_state::ClusterWorkerStateService
    for WorkerServiceHandle<R, V, C>
where
    R: WorkerPaneRuntime + 'static,
    V: WorkerLeaseVerifier + 'static,
    C: crate::ClusterRuntimeOps + Send + Sync + 'static,
{
    fn get<'a>(
        &'a self,
        execution_id: ExecutionId,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<WorkerQueryResult, WorkerServiceError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.registry.get(&execution_id) })
    }

    fn inventory<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WorkerExecutionList> + Send + 'a>> {
        Box::pin(async move {
            self.registry.inventory().unwrap_or(WorkerExecutionList {
                executions: Vec::new(),
            })
        })
    }
}

pub trait WorkerLeaseVerifier: Send + Sync {
    /// Verifies the signature over the canonical authority fields excluding
    /// `lease_signature`.
    ///
    /// # Errors
    ///
    /// Returns a reason when the signature or signer is invalid.
    fn verify(&self, authority: &WorkerAuthority, signed_payload: &[u8]) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AcceptedLease {
    term: u64,
    sequence: u64,
    lease_id: uuid::Uuid,
    accepted_monotonic_ms: u64,
    duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerExecutionAuthority {
    pub cluster_id: String,
    pub workspace_id: uuid::Uuid,
    pub pane_id: uuid::Uuid,
    pub execution_id: ExecutionId,
    pub generation: u64,
    pub local_node_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorkerFenceState {
    accepted: BTreeMap<uuid::Uuid, AcceptedLease>,
}

impl WorkerFenceState {
    /// Validates one worker mutation and advances the per-execution fencing
    /// watermark only after signature and scope checks succeed.
    ///
    /// `monotonic_now_ms` must come from a process-local monotonic clock. A new
    /// process starts with an empty `WorkerFenceState`, invalidating old cached
    /// lease acceptance as required by the lease model.
    ///
    /// # Errors
    ///
    /// Returns a generated worker service error for wrong scope, generation,
    /// audience, operation class, signature, stale term/sequence, or expiry.
    pub fn validate_mutation(
        &mut self,
        expected: &WorkerExecutionAuthority,
        authority: &WorkerAuthority,
        required_class: WorkerOperationClass,
        monotonic_now_ms: u64,
        verifier: &impl WorkerLeaseVerifier,
    ) -> Result<(), WorkerServiceError> {
        validate_scope(expected, authority, required_class)?;
        if authority.lease_duration_ms == 0
            || authority.lease_duration_ms > MAX_CONTROL_LEASE_DURATION_MS
        {
            return Err(WorkerServiceError::InvalidRequest {
                reason: format!(
                    "lease duration must be within 1..={MAX_CONTROL_LEASE_DURATION_MS} ms"
                ),
            });
        }
        let signed = canonical_unsigned_authority(authority)?;
        verifier.verify(authority, &signed).map_err(|reason| {
            WorkerServiceError::InvalidRequest {
                reason: format!("lease signature is invalid: {reason}"),
            }
        })?;

        if let Some(previous) = self.accepted.get(&authority.execution_id.value) {
            if authority.control_term < previous.term {
                return Err(WorkerServiceError::StaleTerm {
                    expected_minimum: previous.term,
                    received: authority.control_term,
                });
            }
            if authority.control_term == previous.term {
                if authority.lease_sequence < previous.sequence {
                    return Err(WorkerServiceError::StaleTerm {
                        expected_minimum: previous.term,
                        received: authority.control_term,
                    });
                }
                if authority.lease_sequence == previous.sequence {
                    if authority.lease_id != previous.lease_id {
                        return Err(WorkerServiceError::AuthorityMismatch);
                    }
                    if monotonic_now_ms
                        >= previous
                            .accepted_monotonic_ms
                            .saturating_add(previous.duration_ms)
                    {
                        return Err(WorkerServiceError::LeaseExpired);
                    }
                    return Ok(());
                }
            }
        }

        self.accepted.insert(
            authority.execution_id.value,
            AcceptedLease {
                term: authority.control_term,
                sequence: authority.lease_sequence,
                lease_id: authority.lease_id,
                accepted_monotonic_ms: monotonic_now_ms,
                duration_ms: authority.lease_duration_ms,
            },
        );
        Ok(())
    }
}

impl WorkerFenceState {
    /// Checks an observe lease: in scope, of an allowed duration, issued recently enough (with
    /// slack for clock differences between nodes), signed by an active voter, and of no older a
    /// term than the last mutation accepted. It records nothing: observing is not ordered with
    /// mutations, so it never stales a mutation lease.
    ///
    /// # Errors
    ///
    /// Returns scope, duration, expiry, signature, or term failures.
    pub fn validate_observation(
        &self,
        expected: &WorkerExecutionAuthority,
        authority: &WorkerAuthority,
        now_unix_ms: u64,
        verifier: &impl WorkerLeaseVerifier,
    ) -> Result<(), WorkerServiceError> {
        validate_scope(expected, authority, WorkerOperationClass::Observe)?;
        if authority.lease_duration_ms == 0
            || authority.lease_duration_ms > MAX_CONTROL_LEASE_DURATION_MS
        {
            return Err(WorkerServiceError::InvalidRequest {
                reason: format!(
                    "lease duration must be within 1..={MAX_CONTROL_LEASE_DURATION_MS} ms"
                ),
            });
        }
        let expires = authority
            .lease_issued_at_unix_ms
            .saturating_add(authority.lease_duration_ms)
            .saturating_add(OBSERVE_CLOCK_SKEW_MS);
        if now_unix_ms >= expires {
            return Err(WorkerServiceError::LeaseExpired);
        }
        let signed = canonical_unsigned_authority(authority)?;
        verifier.verify(authority, &signed).map_err(|reason| {
            WorkerServiceError::InvalidRequest {
                reason: format!("lease signature is invalid: {reason}"),
            }
        })?;
        if let Some(previous) = self.accepted.get(&authority.execution_id.value)
            && authority.control_term < previous.term
        {
            return Err(WorkerServiceError::StaleTerm {
                expected_minimum: previous.term,
                received: authority.control_term,
            });
        }
        Ok(())
    }
}

/// Observes executions for a connection (`cluster-worker-stream`): the worker side of frame
/// replication, kept by the plugin for the service that names the caller's connection.
pub trait ExecutionObserver: Send + Sync {
    /// See [`WorkerRegistry::observe`].
    ///
    /// # Errors
    ///
    /// Returns lease, execution, or local runtime failures.
    fn observe(
        &self,
        client_id: uuid::Uuid,
        authorities: &[WorkerAuthority],
        max_history_lines: u32,
    ) -> Result<Vec<bmux_cluster_plugin_api::cluster_types::ObservedExecution>, WorkerServiceError>;
}

impl<R, V, C> ExecutionObserver for WorkerServiceHandle<R, V, C>
where
    R: WorkerPaneRuntime,
    V: WorkerLeaseVerifier,
    C: Send + Sync,
{
    fn observe(
        &self,
        client_id: uuid::Uuid,
        authorities: &[WorkerAuthority],
        max_history_lines: u32,
    ) -> Result<Vec<bmux_cluster_plugin_api::cluster_types::ObservedExecution>, WorkerServiceError>
    {
        self.registry.observe(
            client_id,
            authorities,
            max_history_lines,
            crate::now_unix_ms(),
        )
    }
}

fn validate_scope(
    expected: &WorkerExecutionAuthority,
    authority: &WorkerAuthority,
    required_class: WorkerOperationClass,
) -> Result<(), WorkerServiceError> {
    if authority.generation != expected.generation {
        return Err(WorkerServiceError::StaleGeneration {
            expected: expected.generation,
            received: authority.generation,
        });
    }
    if authority.cluster_id != expected.cluster_id
        || authority.workspace_id.value != expected.workspace_id
        || authority.pane_id.value != expected.pane_id
        || authority.execution_id != expected.execution_id
        || authority.audience_node_id != expected.local_node_id
        || authority.operation_class != required_class
        || authority.principal_id.trim().is_empty()
        || authority.issuer_node_id.trim().is_empty()
    {
        return Err(WorkerServiceError::AuthorityMismatch);
    }
    Ok(())
}

pub(crate) fn canonical_unsigned_authority(
    authority: &WorkerAuthority,
) -> Result<Vec<u8>, WorkerServiceError> {
    let mut unsigned = authority.clone();
    unsigned.lease_signature.clear();
    bmux_plugin_sdk::encode_service_message(&unsigned).map_err(|error| {
        WorkerServiceError::InvalidRequest {
            reason: format!("authority encoding failed: {error}"),
        }
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCommandDedup<T> {
    outcomes: BTreeMap<uuid::Uuid, (Vec<u8>, T)>,
}

impl<T> Default for WorkerCommandDedup<T> {
    fn default() -> Self {
        Self {
            outcomes: BTreeMap::new(),
        }
    }
}

impl<T: Clone> WorkerCommandDedup<T> {
    /// Returns an existing idempotent outcome or records the newly computed
    /// one. Reusing a command ID with another fingerprint fails closed.
    ///
    /// # Errors
    ///
    /// Returns `command-conflict` when an ID is reused with another request.
    pub fn resolve(
        &mut self,
        command_id: &CommandId,
        fingerprint: Vec<u8>,
        compute: impl FnOnce() -> T,
    ) -> Result<T, WorkerServiceError> {
        if let Some((existing, outcome)) = self.outcomes.get(&command_id.value) {
            return if existing == &fingerprint {
                Ok(outcome.clone())
            } else {
                Err(WorkerServiceError::CommandConflict)
            };
        }
        let outcome = compute();
        self.outcomes
            .insert(command_id.value, (fingerprint, outcome.clone()));
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_cluster_plugin_api::cluster_types::{
        LogicalPaneId, WorkerAdoptionSpec, WorkerAuthority, WorkerLaunchSpec, WorkerOperationClass,
        WorkerSignal, WorkspaceId,
    };
    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct FakePaneRuntime {
        calls: std::sync::Arc<StdMutex<Vec<String>>>,
        existing: std::sync::Arc<StdMutex<BTreeSet<(uuid::Uuid, uuid::Uuid)>>>,
        next: std::sync::Arc<StdMutex<u128>>,
    }

    impl FakePaneRuntime {
        fn binding(&self) -> (uuid::Uuid, uuid::Uuid) {
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let binding = (
                uuid::Uuid::from_u128(100 + *next),
                uuid::Uuid::from_u128(200 + *next),
            );
            drop(next);
            self.existing.lock().unwrap().insert(binding);
            binding
        }
    }

    impl WorkerPaneRuntime for FakePaneRuntime {
        fn launch(
            &self,
            _authority: &WorkerAuthority,
            _spec: &WorkerLaunchSpec,
        ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
            self.calls.lock().unwrap().push("launch".to_string());
            Ok(self.binding())
        }

        fn adopt(
            &self,
            _authority: &WorkerAuthority,
            spec: &WorkerAdoptionSpec,
        ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
            self.calls.lock().unwrap().push("adopt".to_string());
            let binding = (spec.local_session_id, spec.local_pane_id);
            if self.existing.lock().unwrap().contains(&binding) {
                Ok(binding)
            } else {
                Err(WorkerServiceError::LocalRuntime {
                    reason: "local pane is missing".to_string(),
                })
            }
        }

        fn input(
            &self,
            _session_id: uuid::Uuid,
            _pane_id: uuid::Uuid,
            _data: &[u8],
        ) -> Result<(), WorkerServiceError> {
            self.calls.lock().unwrap().push("input".to_string());
            Ok(())
        }

        fn observe(
            &self,
            _client_id: uuid::Uuid,
            _targets: &[(uuid::Uuid, uuid::Uuid)],
            _max_history_lines: u32,
        ) -> Result<(), WorkerServiceError> {
            Ok(())
        }

        fn resize(
            &self,
            _session_id: uuid::Uuid,
            _pane_id: uuid::Uuid,
            _cols: u16,
            _rows: u16,
        ) -> Result<(), WorkerServiceError> {
            self.calls.lock().unwrap().push("resize".to_string());
            Ok(())
        }

        fn signal(
            &self,
            _session_id: uuid::Uuid,
            _pane_id: uuid::Uuid,
            _signal: WorkerSignal,
        ) -> Result<(), WorkerServiceError> {
            self.calls.lock().unwrap().push("signal".to_string());
            Ok(())
        }

        fn restart(
            &self,
            _session_id: uuid::Uuid,
            _pane_id: uuid::Uuid,
            _spec: &WorkerLaunchSpec,
        ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
            self.calls.lock().unwrap().push("restart".to_string());
            Ok(self.binding())
        }

        fn close(
            &self,
            session_id: uuid::Uuid,
            pane_id: uuid::Uuid,
        ) -> Result<(), WorkerServiceError> {
            self.calls.lock().unwrap().push("close".to_string());
            self.existing.lock().unwrap().remove(&(session_id, pane_id));
            Ok(())
        }

        fn contains(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> bool {
            self.existing
                .lock()
                .unwrap()
                .contains(&(session_id, pane_id))
        }
    }

    struct AcceptSignature;

    impl WorkerLeaseVerifier for AcceptSignature {
        fn verify(&self, _authority: &WorkerAuthority, signed: &[u8]) -> Result<(), String> {
            if signed.is_empty() {
                Err("empty signed payload".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn expected() -> WorkerExecutionAuthority {
        WorkerExecutionAuthority {
            cluster_id: "cluster:test".to_string(),
            workspace_id: uuid::Uuid::from_u128(1),
            pane_id: uuid::Uuid::from_u128(2),
            execution_id: ExecutionId {
                value: uuid::Uuid::from_u128(3),
            },
            generation: 4,
            local_node_id: "node:local".to_string(),
        }
    }

    fn authority(term: u64, sequence: u64) -> WorkerAuthority {
        let expected = expected();
        WorkerAuthority {
            cluster_id: expected.cluster_id,
            workspace_id: WorkspaceId {
                value: expected.workspace_id,
            },
            pane_id: LogicalPaneId {
                value: expected.pane_id,
            },
            execution_id: expected.execution_id,
            generation: expected.generation,
            control_term: term,
            lease_sequence: sequence,
            operation_class: WorkerOperationClass::Interactive,
            principal_id: "principal:test".to_string(),
            issuer_node_id: "node:issuer".to_string(),
            audience_node_id: expected.local_node_id,
            lease_id: uuid::Uuid::from_u128(u128::from(term) << 64 | u128::from(sequence)),
            lease_issued_at_unix_ms: 1,
            lease_duration_ms: 5_000,
            lease_signature: vec![1],
        }
    }

    #[test]
    fn fencing_rejects_wrong_generation_scope_audience_and_class() {
        let expected = expected();
        for mutation in 0..4 {
            let mut candidate = authority(1, 1);
            match mutation {
                0 => candidate.generation = 3,
                1 => candidate.pane_id.value = uuid::Uuid::from_u128(99),
                2 => candidate.audience_node_id = "node:other".to_string(),
                3 => candidate.operation_class = WorkerOperationClass::Lifecycle,
                _ => unreachable!(),
            }
            let error = WorkerFenceState::default()
                .validate_mutation(
                    &expected,
                    &candidate,
                    WorkerOperationClass::Interactive,
                    10,
                    &AcceptSignature,
                )
                .unwrap_err();
            assert!(matches!(
                error,
                WorkerServiceError::StaleGeneration { .. } | WorkerServiceError::AuthorityMismatch
            ));
        }
    }

    #[test]
    fn fencing_rejects_stale_terms_sequences_conflicts_and_expiry() {
        let expected = expected();
        let mut state = WorkerFenceState::default();
        state
            .validate_mutation(
                &expected,
                &authority(2, 3),
                WorkerOperationClass::Interactive,
                100,
                &AcceptSignature,
            )
            .unwrap();
        assert!(matches!(
            state.validate_mutation(
                &expected,
                &authority(1, 4),
                WorkerOperationClass::Interactive,
                101,
                &AcceptSignature,
            ),
            Err(WorkerServiceError::StaleTerm { .. })
        ));
        assert!(matches!(
            state.validate_mutation(
                &expected,
                &authority(2, 2),
                WorkerOperationClass::Interactive,
                101,
                &AcceptSignature,
            ),
            Err(WorkerServiceError::StaleTerm { .. })
        ));
        let mut conflicting = authority(2, 3);
        conflicting.lease_id = uuid::Uuid::from_u128(999);
        assert_eq!(
            state.validate_mutation(
                &expected,
                &conflicting,
                WorkerOperationClass::Interactive,
                101,
                &AcceptSignature,
            ),
            Err(WorkerServiceError::AuthorityMismatch)
        );
        assert_eq!(
            state.validate_mutation(
                &expected,
                &authority(2, 3),
                WorkerOperationClass::Interactive,
                5_100,
                &AcceptSignature,
            ),
            Err(WorkerServiceError::LeaseExpired)
        );
    }

    #[test]
    fn higher_term_or_sequence_advances_fence_and_restart_forgets_leases() {
        let expected = expected();
        let mut state = WorkerFenceState::default();
        for lease in [authority(1, 1), authority(1, 2), authority(2, 1)] {
            state
                .validate_mutation(
                    &expected,
                    &lease,
                    WorkerOperationClass::Interactive,
                    10,
                    &AcceptSignature,
                )
                .unwrap();
        }
        let mut restarted = WorkerFenceState::default();
        restarted
            .validate_mutation(
                &expected,
                &authority(1, 1),
                WorkerOperationClass::Interactive,
                20,
                &AcceptSignature,
            )
            .unwrap();
    }

    #[test]
    fn registry_launch_is_idempotent_across_command_and_recovery_retries() {
        let registry =
            WorkerRegistry::new("node:local", FakePaneRuntime::default(), AcceptSignature);
        let mut authority = authority(1, 1);
        authority.operation_class = WorkerOperationClass::Lifecycle;
        let command_id = CommandId {
            value: uuid::Uuid::from_u128(50),
        };
        let spec = WorkerLaunchSpec {
            program: Some("sh".to_string()),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
        };
        let first = registry
            .launch(&command_id, authority.clone(), &spec, 10)
            .unwrap();
        let replay = registry
            .launch(&command_id, authority.clone(), &spec, 11)
            .unwrap();
        assert_eq!(first, replay);
        let recovered = registry
            .launch(
                &CommandId {
                    value: uuid::Uuid::from_u128(51),
                },
                authority,
                &spec,
                12,
            )
            .unwrap();
        assert_eq!(first, recovered);
        assert_eq!(
            registry.runtime.calls.lock().unwrap().as_slice(),
            ["launch"]
        );
        assert_ne!(first.local_pane_id, first.authority.execution_id.value);
        assert!(matches!(
            registry.get(&first.authority.execution_id).unwrap(),
            WorkerQueryResult::Found { .. }
        ));
    }

    #[test]
    fn registry_fences_mutations_before_calling_local_runtime() {
        let registry =
            WorkerRegistry::new("node:local", FakePaneRuntime::default(), AcceptSignature);
        let mut lifecycle = authority(1, 1);
        lifecycle.operation_class = WorkerOperationClass::Lifecycle;
        let execution = registry
            .launch(
                &CommandId {
                    value: uuid::Uuid::from_u128(51),
                },
                lifecycle,
                &WorkerLaunchSpec {
                    program: Some("sh".to_string()),
                    args: Vec::new(),
                    cwd: None,
                    env: Vec::new(),
                    cols: 80,
                    rows: 24,
                },
                10,
            )
            .unwrap();
        let mut interactive = execution.authority;
        interactive.operation_class = WorkerOperationClass::Interactive;
        interactive.lease_sequence = 2;
        interactive.lease_id = uuid::Uuid::from_u128(52);
        registry
            .input(
                &CommandId {
                    value: uuid::Uuid::from_u128(53),
                },
                &interactive,
                b"hello",
                11,
            )
            .unwrap();
        let mut stale = interactive;
        stale.generation -= 1;
        assert!(matches!(
            registry.input(
                &CommandId {
                    value: uuid::Uuid::from_u128(54),
                },
                &stale,
                b"bad",
                12,
            ),
            Err(WorkerServiceError::StaleGeneration { .. })
        ));
        assert_eq!(
            registry.runtime.calls.lock().unwrap().as_slice(),
            ["launch", "input"]
        );
    }

    #[test]
    fn registry_observation_and_reconciliation_classify_current_missing_and_orphaned() {
        fn lifecycle_like(authority: &WorkerAuthority) -> WorkerAuthority {
            let mut other = authority.clone();
            other.operation_class = WorkerOperationClass::Lifecycle;
            other
        }
        let registry =
            WorkerRegistry::new("node:local", FakePaneRuntime::default(), AcceptSignature);
        let mut lifecycle = authority(1, 1);
        lifecycle.operation_class = WorkerOperationClass::Lifecycle;
        let execution = registry
            .launch(
                &CommandId {
                    value: uuid::Uuid::from_u128(55),
                },
                lifecycle,
                &WorkerLaunchSpec {
                    program: Some("sh".to_string()),
                    args: Vec::new(),
                    cwd: None,
                    env: Vec::new(),
                    cols: 80,
                    rows: 24,
                },
                10,
            )
            .unwrap();
        // An observe lease names the execution's local pane; another class, or a lease issued too
        // long ago, observes nothing.
        let mut observe = authority(1, 2);
        observe.operation_class = WorkerOperationClass::Observe;
        let client = uuid::Uuid::from_u128(77);
        let observed = registry
            .observe(client, std::slice::from_ref(&observe), 0, 100)
            .unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].session_id, execution.local_session_id);
        assert_eq!(observed[0].pane_id, execution.local_pane_id);
        assert!(matches!(
            registry.observe(
                client,
                std::slice::from_ref(&lifecycle_like(&observe)),
                0,
                100
            ),
            Err(WorkerServiceError::AuthorityMismatch)
        ));
        assert!(matches!(
            registry.observe(client, &[observe], 0, 1_000_000),
            Err(WorkerServiceError::LeaseExpired)
        ));

        registry.reconcile(vec![execution.clone()]).unwrap();
        let WorkerQueryResult::Found { execution: current } =
            registry.get(&execution.authority.execution_id).unwrap()
        else {
            panic!("current execution should remain in registry");
        };
        assert_eq!(current.state, WorkerExecutionState::Ready);
        assert_eq!(current.authority, execution.authority);

        registry
            .runtime
            .existing
            .lock()
            .unwrap()
            .remove(&(execution.local_session_id, execution.local_pane_id));
        registry.reconcile(vec![execution.clone()]).unwrap();
        let WorkerQueryResult::Found { execution: missing } =
            registry.get(&execution.authority.execution_id).unwrap()
        else {
            panic!("execution should remain in registry");
        };
        assert_eq!(missing.state, WorkerExecutionState::Unavailable);
        assert_eq!(missing.authority, execution.authority);
        assert_eq!(registry.reconcile(Vec::new()).unwrap().execution_count, 1);
        let WorkerQueryResult::Found { execution: orphan } =
            registry.get(&execution.authority.execution_id).unwrap()
        else {
            panic!("orphan should remain inspectable");
        };
        assert_eq!(orphan.state, WorkerExecutionState::Quarantined);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn durable_worker_registry_restores_and_reconciles_after_restart() {
        #[derive(Default)]
        struct MemoryStorage {
            value: Mutex<Option<Vec<u8>>>,
        }

        impl crate::ClusterRuntimeOps for MemoryStorage {
            fn remote_probe_status(&self, _: crate::HealthProbe, _: &str) -> Result<i32, String> {
                Err("unused".to_string())
            }
            fn session_list(&self) -> Result<crate::SessionListResponse, String> {
                Err("unused".to_string())
            }
            fn session_create(
                &self,
                _: &crate::SessionCreateRequest,
            ) -> Result<crate::SessionCreateResponse, String> {
                Err("unused".to_string())
            }
            fn session_select(
                &self,
                _: &crate::SessionSelectRequest,
            ) -> Result<crate::SessionSelectResponse, String> {
                Err("unused".to_string())
            }
            fn pane_list(
                &self,
                _: &crate::PaneListRequest,
            ) -> Result<crate::PaneListResponse, String> {
                Err("unused".to_string())
            }
            fn pane_launch(
                &self,
                _: &crate::PaneLaunchRequest,
            ) -> Result<crate::PaneLaunchResponse, String> {
                Err("unused".to_string())
            }
            fn pane_close(
                &self,
                _: &crate::PaneCloseRequest,
            ) -> Result<crate::PaneCloseResponse, String> {
                Err("unused".to_string())
            }
            fn storage_get(
                &self,
                _: &bmux_plugin_sdk::StorageGetRequest,
            ) -> Result<bmux_plugin_sdk::StorageGetResponse, String> {
                Ok(bmux_plugin_sdk::StorageGetResponse {
                    value: self.value.lock().unwrap().clone(),
                })
            }
            fn storage_set(
                &self,
                request: &bmux_plugin_sdk::StorageSetRequest,
            ) -> Result<(), String> {
                *self.value.lock().unwrap() = Some(request.value.clone());
                Ok(())
            }
        }

        let storage = std::sync::Arc::new(MemoryStorage::default());
        let runtime = FakePaneRuntime::default();
        let registry = std::sync::Arc::new(WorkerRegistry::new(
            "node:local",
            runtime.clone(),
            AcceptSignature,
        ));
        let mut lifecycle = authority(1, 1);
        lifecycle.operation_class = WorkerOperationClass::Lifecycle;
        let execution = registry
            .launch(
                &CommandId {
                    value: uuid::Uuid::from_u128(90),
                },
                lifecycle,
                &WorkerLaunchSpec {
                    program: Some("sh".to_string()),
                    args: Vec::new(),
                    cwd: None,
                    env: Vec::new(),
                    cols: 80,
                    rows: 24,
                },
                1,
            )
            .unwrap();
        let durable = DurableWorkerRegistry::new(registry, storage.clone());
        durable.persist().unwrap();

        let restored =
            std::sync::Arc::new(WorkerRegistry::new("node:local", runtime, AcceptSignature));
        let durable = DurableWorkerRegistry::new(restored.clone(), storage);
        let stats = durable.restore_and_reconcile().unwrap();
        assert_eq!(stats.execution_count, 1);
        let WorkerQueryResult::Found {
            execution: recovered,
        } = restored.get(&execution.authority.execution_id).unwrap()
        else {
            panic!("execution should restore");
        };
        assert_eq!(recovered.state, WorkerExecutionState::Ready);
        assert_eq!(recovered.authority, execution.authority);
    }

    #[test]
    fn worker_command_dedup_replays_identical_outcomes_and_rejects_conflicts() {
        let id = CommandId {
            value: uuid::Uuid::from_u128(1),
        };
        let mut dedup = WorkerCommandDedup::default();
        assert_eq!(dedup.resolve(&id, vec![1], || 7).unwrap(), 7);
        assert_eq!(dedup.resolve(&id, vec![1], || 8).unwrap(), 7);
        assert_eq!(
            dedup.resolve(&id, vec![2], || 9),
            Err(WorkerServiceError::CommandConflict)
        );
    }
}
