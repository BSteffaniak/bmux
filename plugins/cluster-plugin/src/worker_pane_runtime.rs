//! Pane-runtime adapter used by the cluster worker registry.
//!
//! Every operation goes through generated pane-runtime clients over an owned
//! host caller, so production and tests share exactly the same transport
//! behavior.

use crate::worker_runtime::{WorkerLeaseVerifier, WorkerPaneRuntime, WorkerRegistry};
use bmux_cluster_plugin_api::cluster_types::{
    WorkerAdoptionSpec, WorkerAuthority, WorkerLaunchSpec, WorkerServiceError, WorkerSignal,
};
use bmux_pane_runtime_plugin_api::{attach_runtime_state, pane_runtime_commands};
use bmux_plugin::TypedServiceCaller;
use std::sync::Arc;

tokio::task_local! {
    /// The caller of the service invocation being served, while it is served.
    static INVOCATION_CALLER: Arc<TypedServiceCaller>;
}

/// Run `work` as the body of the service invocation `caller` was captured from.
///
/// Pane-runtime calls the worker makes while serving it carry that invocation's
/// authority and provenance, which pane runtime authorizes; the caller captured
/// at registration belongs to no invocation and is refused.
pub fn serving_invocation<R>(caller: Arc<TypedServiceCaller>, work: impl FnOnce() -> R) -> R {
    INVOCATION_CALLER.sync_scope(caller, work)
}

/// The local pane an execution runs in. Each execution of a logical pane gets
/// its own: pane-runtime state keyed by pane (such as an application's
/// resurrection checkpoint) belongs to one execution and must not be found by
/// the next generation. Derived, so relaunching an execution finds its pane.
fn execution_pane_id(execution_id: uuid::Uuid) -> uuid::Uuid {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"bmux.cluster.execution-pane.v1\0");
    digest.update(execution_id.as_bytes());
    let bytes: [u8; 16] = digest.finalize()[..16]
        .try_into()
        .expect("SHA-256 prefix length");
    uuid::Uuid::from_bytes(bytes)
}

pub struct TypedPaneRuntime {
    caller: Arc<TypedServiceCaller>,
}

impl TypedPaneRuntime {
    #[must_use]
    pub const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }

    /// The invocation being served, else the caller this runtime was built with.
    fn caller(&self) -> Arc<TypedServiceCaller> {
        INVOCATION_CALLER
            .try_with(Arc::clone)
            .unwrap_or_else(|_| Arc::clone(&self.caller))
    }

    fn dispatch_error(
        operation: &'static str,
        error: impl std::fmt::Display,
    ) -> WorkerServiceError {
        WorkerServiceError::LocalRuntime {
            reason: format!("pane-runtime {operation} dispatch failed: {error}"),
        }
    }

    fn service_error(operation: &'static str, error: impl std::fmt::Debug) -> WorkerServiceError {
        WorkerServiceError::LocalRuntime {
            reason: format!("pane-runtime {operation} failed: {error:?}"),
        }
    }

    const fn pane_signal(signal: WorkerSignal) -> pane_runtime_commands::PaneProcessSignal {
        match signal {
            WorkerSignal::Interrupt => pane_runtime_commands::PaneProcessSignal::Interrupt,
            WorkerSignal::Terminate => pane_runtime_commands::PaneProcessSignal::Terminate,
            WorkerSignal::Kill => pane_runtime_commands::PaneProcessSignal::Kill,
            WorkerSignal::Hangup => pane_runtime_commands::PaneProcessSignal::Hangup,
        }
    }
}

impl WorkerPaneRuntime for TypedPaneRuntime {
    fn launch(
        &self,
        authority: &WorkerAuthority,
        spec: &WorkerLaunchSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        if spec.cols == 0 || spec.rows == 0 {
            return Err(WorkerServiceError::InvalidRequest {
                reason: "worker launch dimensions must be positive".to_string(),
            });
        }
        let program = spec
            .program
            .as_deref()
            .map(str::trim)
            .filter(|program| !program.is_empty())
            .ok_or_else(|| WorkerServiceError::InvalidRequest {
                reason: "worker launch requires an explicit program".to_string(),
            })?;
        let session_id = authority.execution_id.value;
        let pane_id = execution_pane_id(session_id);
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        let launch = bmux_plugin::block_on_typed_dispatch(
            pane_runtime_commands::client::ensure_execution_pane(
                &mut client,
                session_id,
                pane_id,
                Some(format!("cluster:{}", authority.pane_id.value)),
                program.to_string(),
                spec.args.clone(),
                spec.cwd.clone(),
                spec.env
                    .iter()
                    .map(|entry| pane_runtime_commands::EnvironmentEntry {
                        key: entry.key.clone(),
                        value: entry.value.clone(),
                    })
                    .collect(),
                spec.rows,
                spec.cols,
            ),
        )
        .map_err(|error| Self::dispatch_error("ensure execution pane", error))?;
        launch.map_err(|error| Self::service_error("ensure execution pane", error))?;
        Ok((session_id, pane_id))
    }

    fn adopt(
        &self,
        _authority: &WorkerAuthority,
        spec: &WorkerAdoptionSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        let result = bmux_plugin::block_on_typed_dispatch(
            bmux_pane_runtime_plugin_api::pane_runtime_state::client::get_pane(
                &mut client,
                spec.local_session_id,
                spec.local_pane_id,
            ),
        )
        .map_err(|error| Self::dispatch_error("adopt query", error))?;
        result.map_err(|error| Self::service_error("adopt query", error))?;
        Ok((spec.local_session_id, spec.local_pane_id))
    }

    fn input(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        data: &[u8],
    ) -> Result<(), WorkerServiceError> {
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        bmux_plugin::block_on_typed_dispatch(pane_runtime_commands::client::execution_pane_input(
            &mut client,
            session_id,
            pane_id,
            data.to_vec(),
        ))
        .map_err(|error| Self::dispatch_error("input", error))?
        .map(|_| ())
        .map_err(|error| Self::service_error("input", error))
    }

    fn observe(
        &self,
        client_id: uuid::Uuid,
        targets: &[(uuid::Uuid, uuid::Uuid)],
        max_history_lines: u32,
    ) -> Result<(), WorkerServiceError> {
        // The cluster plugin itself, in process, names the observing client;
        // pane runtime admits frame observers only from it, never from the
        // invocation being served.
        let caller = Arc::clone(&self.caller);
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        bmux_plugin::block_on_typed_dispatch(attach_runtime_state::client::observe_pane_frames(
            &mut client,
            client_id,
            targets
                .iter()
                .map(|(session_id, pane_id)| attach_runtime_state::ObservedPane {
                    session_id: *session_id,
                    pane_id: *pane_id,
                })
                .collect(),
            max_history_lines,
        ))
        .map_err(|error| Self::dispatch_error("observe", error))?
        .map(|_| ())
        .map_err(|error| Self::service_error("observe", error))
    }

    fn resize(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<(), WorkerServiceError> {
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        bmux_plugin::block_on_typed_dispatch(pane_runtime_commands::client::pane_set_pty_size(
            &mut client,
            session_id,
            pane_id,
            rows,
            cols,
        ))
        .map_err(|error| Self::dispatch_error("resize", error))?
        .map(|_| ())
        .map_err(|error| Self::service_error("resize", error))
    }

    fn signal(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        signal: WorkerSignal,
    ) -> Result<(), WorkerServiceError> {
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        bmux_plugin::block_on_typed_dispatch(pane_runtime_commands::client::pane_signal(
            &mut client,
            session_id,
            pane_id,
            Self::pane_signal(signal),
        ))
        .map_err(|error| Self::dispatch_error("signal", error))?
        .map(|_| ())
        .map_err(|error| Self::service_error("signal", error))
    }

    fn restart(
        &self,
        _session_id: uuid::Uuid,
        _pane_id: uuid::Uuid,
        _spec: &WorkerLaunchSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        Err(WorkerServiceError::Unavailable {
            reason: "in-place restart is prohibited; commit a replacement execution".to_string(),
        })
    }

    fn close(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> Result<(), WorkerServiceError> {
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        bmux_plugin::block_on_typed_dispatch(pane_runtime_commands::client::destroy_execution_pane(
            &mut client,
            session_id,
            pane_id,
        ))
        .map_err(|error| Self::dispatch_error("close", error))?
        .map(|_| ())
        .map_err(|error| Self::service_error("close", error))
    }

    fn contains(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> bool {
        let caller = self.caller();
        let mut client = bmux_plugin::ServiceCallerDispatchClient::new(caller.as_ref());
        bmux_plugin::block_on_typed_dispatch(
            bmux_pane_runtime_plugin_api::pane_runtime_state::client::get_pane(
                &mut client,
                session_id,
                pane_id,
            ),
        )
        .is_ok_and(|result| result.is_ok())
    }
}

#[must_use]
pub fn local_worker_registry<V>(
    caller: Arc<TypedServiceCaller>,
    local_node_id: impl Into<String>,
    verifier: V,
) -> Arc<WorkerRegistry<TypedPaneRuntime, V>>
where
    V: WorkerLeaseVerifier,
{
    Arc::new(WorkerRegistry::new(
        local_node_id,
        TypedPaneRuntime::new(caller),
        verifier,
    ))
}
