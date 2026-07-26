//! Worker-side fencing and bounded execution-output primitives.
//!
//! Concrete pane-runtime orchestration owns local PTY bindings. These helpers
//! enforce the cluster execution authority and cursor invariants independently
//! of that backend so retries and restart reconciliation use one implementation.

use bmux_cluster_plugin_api::cluster_types::{
    CommandId, ExecutionId, WorkerAuthority, WorkerExecution, WorkerExecutionList,
    WorkerExecutionState, WorkerLaunchResult, WorkerOperationClass, WorkerOutput,
    WorkerQueryResult, WorkerRegistryStats, WorkerServiceError,
};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

pub const MAX_CONTROL_LEASE_DURATION_MS: u64 = 5_000;
const DEFAULT_REGISTRY_OUTPUT_HARD_MAX_BYTES: usize = 256 * 1024 * 1024;

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

    /// Reads an absolute output cursor range from the exact local pane.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when the binding is missing or output
    /// cannot be read.
    fn output(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cursor: u64,
        max_bytes: u32,
    ) -> Result<bmux_pane_runtime_state::OutputRead, WorkerServiceError>;

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

    /// Returns a complete terminal snapshot and stream cursor for repair.
    ///
    /// # Errors
    ///
    /// Returns a generated worker error when the runtime cannot snapshot.
    fn snapshot(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
    ) -> Result<(u64, Vec<u8>), WorkerServiceError>;

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
    output: WorkerOutputBuffer,
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
                    output: WorkerOutputBuffer::new(
                        execution.authority.execution_id.clone(),
                        execution.authority.generation,
                        WorkerOutputRetention::default(),
                    ),
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
                output: WorkerOutputBuffer::new(
                    execution.authority.execution_id.clone(),
                    execution.authority.generation,
                    WorkerOutputRetention::default(),
                ),
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

    /// Appends local pane output to the execution's bounded cursor stream.
    ///
    /// # Errors
    ///
    /// Returns missing execution, stale generation, cursor, or lock failures.
    pub fn append_output(
        &self,
        execution_id: &ExecutionId,
        generation: u64,
        data: Vec<u8>,
        monotonic_now_ms: u64,
    ) -> Result<u64, WorkerServiceError> {
        let mut inner = self.lock()?;
        let result = {
            let entry = execution_entry_mut(&mut inner.executions, execution_id, generation)?;
            let end = entry.output.append(data, monotonic_now_ms)?;
            entry.execution.output_end = end;
            entry.execution.output_start = entry.output.start;
            enforce_registry_output_cap(
                &mut inner.executions,
                DEFAULT_REGISTRY_OUTPUT_HARD_MAX_BYTES,
            );
            Ok(end)
        };
        drop(inner);
        result
    }

    /// Reads one bounded output batch for the exact generation.
    ///
    /// # Errors
    ///
    /// Returns missing execution, stale generation, or lock failures.
    pub fn output(
        &self,
        execution_id: &ExecutionId,
        generation: u64,
        cursor: u64,
        max_bytes: u32,
    ) -> Result<WorkerOutput, WorkerServiceError> {
        let inner = self.lock()?;
        let execution = execution_entry(&inner.executions, execution_id, generation)?
            .execution
            .clone();
        drop(inner);
        let read = self.runtime.output(
            execution.local_session_id,
            execution.local_pane_id,
            cursor,
            max_bytes,
        )?;
        Ok(WorkerOutput {
            execution_id: execution_id.clone(),
            generation,
            requested_cursor: cursor,
            retained_start: read.retained_start,
            next_cursor: read.stream_end,
            gap: read.stream_gap,
            output_still_pending: read.stream_end < read.source_end,
            data: read.bytes,
        })
    }

    /// Produces a complete local terminal snapshot for cursor-gap repair.
    ///
    /// # Errors
    ///
    /// Returns missing execution, stale generation, local runtime, or lock failures.
    pub fn snapshot(
        &self,
        execution_id: &ExecutionId,
        generation: u64,
    ) -> Result<bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot, WorkerServiceError>
    {
        let inner = self.lock()?;
        let entry = execution_entry(&inner.executions, execution_id, generation)?;
        let execution = entry.execution.clone();
        drop(inner);
        let (cursor, encoded) = self
            .runtime
            .snapshot(execution.local_session_id, execution.local_pane_id)?;
        Ok(
            bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot {
                execution_id: execution_id.clone(),
                generation,
                cursor,
                encoded,
            },
        )
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
                    output: WorkerOutputBuffer::new(
                        execution.authority.execution_id.clone(),
                        execution.authority.generation,
                        WorkerOutputRetention::default(),
                    ),
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
        output_start: 0,
        output_end: 0,
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

fn execution_entry_mut<'a>(
    executions: &'a mut BTreeMap<uuid::Uuid, WorkerRegistryEntry>,
    execution_id: &ExecutionId,
    generation: u64,
) -> Result<&'a mut WorkerRegistryEntry, WorkerServiceError> {
    let entry =
        executions
            .get_mut(&execution_id.value)
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
        retained_output_bytes: executions
            .values()
            .map(|entry| u64::try_from(entry.output.retained_bytes).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add),
    }
}

fn enforce_registry_output_cap(
    executions: &mut BTreeMap<uuid::Uuid, WorkerRegistryEntry>,
    max_bytes: usize,
) {
    let total = executions
        .values()
        .map(|entry| entry.output.retained_bytes)
        .fold(0_usize, usize::saturating_add);
    if total <= max_bytes {
        return;
    }
    let mut remaining = max_bytes;
    for entry in executions.values_mut().rev() {
        let allowance = remaining.min(entry.output.retained_bytes);
        entry.output.enforce_global_cap(allowance);
        entry.execution.output_start = entry.output.start;
        remaining = remaining.saturating_sub(entry.output.retained_bytes);
    }
}

pub struct NodeSignatureLeaseVerifier<C> {
    caller: std::sync::Arc<C>,
}

impl<C> NodeSignatureLeaseVerifier<C> {
    #[must_use]
    pub const fn new(caller: std::sync::Arc<C>) -> Self {
        Self { caller }
    }
}

impl<C> WorkerLeaseVerifier for NodeSignatureLeaseVerifier<C>
where
    C: crate::ClusterRuntimeOps + Send + Sync,
{
    fn verify(&self, authority: &WorkerAuthority, payload: &[u8]) -> Result<(), String> {
        let membership = crate::membership::load_membership_state(self.caller.as_ref())?
            .ok_or_else(|| "cluster membership is not initialized".to_string())?;
        let issuer = membership
            .members
            .get(&authority.issuer_node_id)
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

    fn output<'a>(
        &'a self,
        execution_id: ExecutionId,
        generation: u64,
        cursor: u64,
        max_bytes: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<WorkerOutput, WorkerServiceError>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.registry
                .output(&execution_id, generation, cursor, max_bytes)
        })
    }

    fn snapshot<'a>(
        &'a self,
        execution_id: ExecutionId,
        generation: u64,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        bmux_cluster_plugin_api::cluster_types::WorkerTerminalSnapshot,
                        WorkerServiceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move { self.registry.snapshot(&execution_id, generation) })
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

fn canonical_unsigned_authority(
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
struct OutputChunk {
    start: u64,
    end: u64,
    observed_monotonic_ms: u64,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerOutputRetention {
    pub minimum_bytes: usize,
    pub minimum_duration_ms: u64,
    pub hard_max_bytes: usize,
}

impl Default for WorkerOutputRetention {
    fn default() -> Self {
        Self {
            minimum_bytes: 16 * 1024 * 1024,
            minimum_duration_ms: 60_000,
            hard_max_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkerOutputBuffer {
    execution_id: ExecutionId,
    generation: u64,
    retention: WorkerOutputRetention,
    chunks: VecDeque<OutputChunk>,
    retained_bytes: usize,
    start: u64,
    end: u64,
}

impl WorkerOutputBuffer {
    #[must_use]
    pub const fn new(
        execution_id: ExecutionId,
        generation: u64,
        retention: WorkerOutputRetention,
    ) -> Self {
        Self {
            execution_id,
            generation,
            retention,
            chunks: VecDeque::new(),
            retained_bytes: 0,
            start: 0,
            end: 0,
        }
    }

    /// Appends one contiguous output segment and returns its exclusive cursor.
    ///
    /// # Errors
    ///
    /// Returns an error if the monotonic cursor would overflow.
    pub fn append(
        &mut self,
        data: Vec<u8>,
        monotonic_now_ms: u64,
    ) -> Result<u64, WorkerServiceError> {
        if data.is_empty() {
            return Ok(self.end);
        }
        let length = u64::try_from(data.len()).map_err(|_| WorkerServiceError::InvalidRequest {
            reason: "output segment length does not fit u64".to_string(),
        })?;
        let end = self
            .end
            .checked_add(length)
            .ok_or_else(|| WorkerServiceError::Unavailable {
                reason: "execution output cursor overflowed".to_string(),
            })?;
        let start = self.end;
        self.end = end;
        self.retained_bytes = self.retained_bytes.saturating_add(data.len());
        self.chunks.push_back(OutputChunk {
            start,
            end,
            observed_monotonic_ms: monotonic_now_ms,
            data,
        });
        self.evict(monotonic_now_ms);
        Ok(end)
    }

    /// Reads a bounded cursor range. Requests older than retention return an
    /// explicit gap and no bytes so callers repair from a complete snapshot.
    #[must_use]
    pub fn read(&self, cursor: u64, max_bytes: u32) -> WorkerOutput {
        if cursor < self.start || cursor > self.end {
            return WorkerOutput {
                execution_id: self.execution_id.clone(),
                generation: self.generation,
                requested_cursor: cursor,
                retained_start: self.start,
                next_cursor: self.start,
                gap: true,
                output_still_pending: self.start < self.end,
                data: Vec::new(),
            };
        }
        let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let mut data = Vec::with_capacity(limit.min(self.retained_bytes));
        let mut next = cursor;
        for chunk in &self.chunks {
            if chunk.end <= cursor || data.len() >= limit {
                continue;
            }
            let offset = usize::try_from(cursor.saturating_sub(chunk.start))
                .unwrap_or(usize::MAX)
                .min(chunk.data.len());
            let available = &chunk.data[offset..];
            let take = available.len().min(limit.saturating_sub(data.len()));
            data.extend_from_slice(&available[..take]);
            next = next.saturating_add(u64::try_from(take).unwrap_or(u64::MAX));
            if take < available.len() {
                break;
            }
        }
        WorkerOutput {
            execution_id: self.execution_id.clone(),
            generation: self.generation,
            requested_cursor: cursor,
            retained_start: self.start,
            next_cursor: next,
            gap: false,
            output_still_pending: next < self.end,
            data,
        }
    }

    /// Forces eviction to a node-wide cap. Subsequent old cursors observe a
    /// gap; snapshot repair remains the only recovery path.
    pub fn enforce_global_cap(&mut self, max_bytes: usize) {
        while self.retained_bytes > max_bytes {
            if !self.pop_front() {
                break;
            }
        }
    }

    fn evict(&mut self, monotonic_now_ms: u64) {
        while self.retained_bytes > self.retention.hard_max_bytes {
            if !self.pop_front() {
                break;
            }
        }
        while let Some(front) = self.chunks.front() {
            let old_enough = monotonic_now_ms.saturating_sub(front.observed_monotonic_ms)
                >= self.retention.minimum_duration_ms;
            let above_minimum = self.retained_bytes.saturating_sub(front.data.len())
                >= self.retention.minimum_bytes;
            if !old_enough || !above_minimum {
                break;
            }
            self.pop_front();
        }
    }

    fn pop_front(&mut self) -> bool {
        let Some(front) = self.chunks.pop_front() else {
            return false;
        };
        self.retained_bytes = self.retained_bytes.saturating_sub(front.data.len());
        self.start = front.end;
        true
    }
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

        fn output(
            &self,
            _session_id: uuid::Uuid,
            _pane_id: uuid::Uuid,
            cursor: u64,
            max_bytes: u32,
        ) -> Result<bmux_pane_runtime_state::OutputRead, WorkerServiceError> {
            let data = b"data";
            let start = usize::try_from(cursor.min(data.len() as u64)).unwrap();
            let end = start
                .saturating_add(usize::try_from(max_bytes).unwrap())
                .min(data.len());
            Ok(bmux_pane_runtime_state::OutputRead {
                bytes: data[start..end].to_vec(),
                retained_start: 0,
                stream_start: cursor,
                stream_end: u64::try_from(end).unwrap(),
                source_end: u64::try_from(data.len()).unwrap(),
                stream_gap: false,
            })
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

        fn snapshot(
            &self,
            _session_id: uuid::Uuid,
            _pane_id: uuid::Uuid,
        ) -> Result<(u64, Vec<u8>), WorkerServiceError> {
            Ok((7, vec![1, 2, 3]))
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
    fn output_is_monotonic_bounded_and_reports_cursor_gaps() {
        let mut output = WorkerOutputBuffer::new(
            ExecutionId {
                value: uuid::Uuid::from_u128(3),
            },
            4,
            WorkerOutputRetention {
                minimum_bytes: 4,
                minimum_duration_ms: 10,
                hard_max_bytes: 8,
            },
        );
        assert_eq!(output.append(b"abcd".to_vec(), 0).unwrap(), 4);
        assert_eq!(output.append(b"efgh".to_vec(), 1).unwrap(), 8);
        let first = output.read(0, 3);
        assert_eq!(first.data, b"abc");
        assert_eq!(first.next_cursor, 3);
        assert!(first.output_still_pending);
        output.append(b"ijkl".to_vec(), 20).unwrap();
        let gap = output.read(0, 10);
        assert!(gap.gap);
        assert_eq!(gap.retained_start, 8);
        let repaired = output.read(8, 8);
        assert_eq!(repaired.data, b"ijkl");
        assert_eq!(repaired.next_cursor, 12);
    }

    #[test]
    fn global_cap_forces_explicit_gap_without_fabricating_continuity() {
        let mut output = WorkerOutputBuffer::new(
            ExecutionId {
                value: uuid::Uuid::from_u128(3),
            },
            4,
            WorkerOutputRetention {
                minimum_bytes: 100,
                minimum_duration_ms: 100,
                hard_max_bytes: 100,
            },
        );
        output.append(b"first".to_vec(), 0).unwrap();
        output.append(b"second".to_vec(), 0).unwrap();
        output.enforce_global_cap(6);
        let gap = output.read(0, 100);
        assert!(gap.gap);
        assert_eq!(gap.retained_start, 5);
        assert_eq!(output.read(5, 100).data, b"second");
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
    fn registry_wide_output_cap_evicts_oldest_execution_streams() {
        let mut executions = BTreeMap::new();
        for id in 1..=2 {
            let mut authority = authority(1, id);
            authority.execution_id.value = uuid::Uuid::from_u128(3 + u128::from(id));
            let mut output = WorkerOutputBuffer::new(
                authority.execution_id.clone(),
                authority.generation,
                WorkerOutputRetention {
                    minimum_bytes: 6,
                    minimum_duration_ms: u64::MAX,
                    hard_max_bytes: usize::MAX,
                },
            );
            output
                .append(vec![u8::try_from(id).unwrap(); 6], id)
                .unwrap();
            executions.insert(
                authority.execution_id.value,
                WorkerRegistryEntry {
                    execution: worker_execution(
                        authority,
                        uuid::Uuid::from_u128(100 + u128::from(id)),
                        uuid::Uuid::from_u128(200 + u128::from(id)),
                    ),
                    output,
                },
            );
        }
        enforce_registry_output_cap(&mut executions, 8);
        assert_eq!(registry_stats(&executions).retained_output_bytes, 6);
        let entries = executions.values().collect::<Vec<_>>();
        assert_eq!(entries[0].output.retained_bytes, 0);
        assert_eq!(entries[0].execution.output_start, 6);
        assert_eq!(entries[1].output.retained_bytes, 6);
    }

    #[test]
    fn registry_output_snapshot_and_reconciliation_classify_current_missing_and_orphaned() {
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
        registry
            .append_output(
                &execution.authority.execution_id,
                execution.authority.generation,
                b"data".to_vec(),
                11,
            )
            .unwrap();
        let output = registry
            .output(
                &execution.authority.execution_id,
                execution.authority.generation,
                0,
                4,
            )
            .unwrap();
        assert_eq!(output.data, b"data");
        let snapshot = registry
            .snapshot(
                &execution.authority.execution_id,
                execution.authority.generation,
            )
            .unwrap();
        assert_eq!(snapshot.cursor, 7);
        assert_eq!(snapshot.encoded, vec![1, 2, 3]);

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
            fn core_cli_command_run_path(
                &self,
                _: &bmux_plugin_sdk::CoreCliCommandRequest,
            ) -> Result<bmux_plugin_sdk::CoreCliCommandResponse, String> {
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
