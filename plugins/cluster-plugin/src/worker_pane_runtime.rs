//! Adapter from the private worker registry to pane-runtime's neutral state API.
//!
//! Local binding IDs are deterministic projections of already-committed
//! execution intent. Retries after an ambiguous local effect therefore reopen
//! the same binding instead of launching a second process.

use crate::worker_runtime::{WorkerLeaseVerifier, WorkerPaneRuntime, WorkerRegistry};
use bmux_cluster_plugin_api::cluster_types::{
    WorkerAdoptionSpec, WorkerAuthority, WorkerLaunchSpec, WorkerServiceError, WorkerSignal,
};
use bmux_pane_runtime_state::{
    PaneLaunchSpec, PaneLayoutNode, PaneResurrectionSnapshot, PaneRuntimeMeta,
    SessionRuntimeManagerHandle,
};
use bmux_session_models::SessionId;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Default)]
pub struct LocalPaneRuntime;

impl LocalPaneRuntime {
    fn runtime() -> Result<SessionRuntimeManagerHandle, WorkerServiceError> {
        bmux_plugin::global_plugin_state_registry()
            .get::<SessionRuntimeManagerHandle>()
            .and_then(|handle| handle.read().ok().map(|guard| (*guard).clone()))
            .ok_or_else(|| WorkerServiceError::Unavailable {
                reason: "pane-runtime manager handle not registered".to_string(),
            })
    }

    fn launch_binding(
        authority: &WorkerAuthority,
        spec: &WorkerLaunchSpec,
        program: &str,
        session_id: SessionId,
        pane_id: uuid::Uuid,
    ) -> Result<(), WorkerServiceError> {
        let pane = PaneRuntimeMeta {
            id: pane_id,
            name: Some(format!("cluster:{}", authority.pane_id.value)),
            shell: program.to_string(),
            launch: Some(PaneLaunchSpec {
                program: program.to_string(),
                args: spec.args.clone(),
                cwd: spec.cwd.clone(),
                env: spec
                    .env
                    .iter()
                    .map(|entry| (entry.key.clone(), entry.value.clone()))
                    .collect::<BTreeMap<_, _>>(),
            }),
            resurrection: PaneResurrectionSnapshot::default(),
        };
        let runtime = Self::runtime()?;
        if runtime.0.session_exists(session_id) {
            if runtime
                .0
                .pane_process_identity(session_id, pane_id)
                .is_some()
            {
                return Ok(());
            }
            return Err(WorkerServiceError::LocalRuntime {
                reason: "deterministic execution session exists without its committed pane"
                    .to_string(),
            });
        }
        runtime
            .0
            .restore_runtime(
                session_id,
                &[pane],
                Some(PaneLayoutNode::Leaf { pane_id }),
                pane_id,
                Vec::new(),
                None,
            )
            .map_err(|error| Self::local_error("launch", error))?;
        runtime
            .0
            .set_pane_pty_size(session_id, pane_id, spec.rows, spec.cols)
            .map_err(|error| Self::local_error("initial resize", error))
    }

    fn local_error(operation: &'static str, error: impl std::fmt::Display) -> WorkerServiceError {
        WorkerServiceError::LocalRuntime {
            reason: format!("pane-runtime {operation} failed: {error}"),
        }
    }
}

impl WorkerPaneRuntime for LocalPaneRuntime {
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
        let session_id = SessionId(authority.execution_id.value);
        let pane_id = authority.pane_id.value;
        Self::launch_binding(authority, spec, program, session_id, pane_id)?;
        Ok((session_id.0, pane_id))
    }

    fn adopt(
        &self,
        _authority: &WorkerAuthority,
        spec: &WorkerAdoptionSpec,
    ) -> Result<(uuid::Uuid, uuid::Uuid), WorkerServiceError> {
        if self.contains(spec.local_session_id, spec.local_pane_id) {
            Ok((spec.local_session_id, spec.local_pane_id))
        } else {
            Err(WorkerServiceError::LocalRuntime {
                reason: "adoption binding does not exist".to_string(),
            })
        }
    }

    fn input(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        data: &[u8],
    ) -> Result<(), WorkerServiceError> {
        Self::runtime()?
            .0
            .write_input_to_pane(
                SessionId(session_id),
                bmux_session_models::ClientId(uuid::Uuid::nil()),
                pane_id,
                data.to_vec(),
            )
            .map(|_| ())
            .map_err(|error| Self::local_error("input", error))
    }

    fn output(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cursor: u64,
        max_bytes: u32,
    ) -> Result<bmux_pane_runtime_state::OutputRead, WorkerServiceError> {
        Self::runtime()?
            .0
            .read_pane_output_at(
                SessionId(session_id),
                pane_id,
                cursor,
                usize::try_from(max_bytes).unwrap_or(usize::MAX),
            )
            .map_err(|error| Self::local_error("output", error))
    }

    fn resize(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<(), WorkerServiceError> {
        if cols == 0 || rows == 0 {
            return Err(WorkerServiceError::InvalidRequest {
                reason: "worker resize dimensions must be positive".to_string(),
            });
        }
        Self::runtime()?
            .0
            .set_pane_pty_size(SessionId(session_id), pane_id, rows, cols)
            .map_err(|error| Self::local_error("resize", error))
    }

    fn signal(
        &self,
        _session_id: uuid::Uuid,
        _pane_id: uuid::Uuid,
        _signal: WorkerSignal,
    ) -> Result<(), WorkerServiceError> {
        Err(WorkerServiceError::Unavailable {
            reason: "pane-runtime process signaling is not available".to_string(),
        })
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
        let runtime = Self::runtime()?;
        let removed = runtime
            .0
            .remove_runtime(SessionId(session_id))
            .ok_or_else(|| WorkerServiceError::LocalRuntime {
                reason: format!("pane {pane_id} runtime is missing"),
            })?;
        runtime.0.shutdown_removed_runtime(removed);
        Ok(())
    }

    fn snapshot(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
    ) -> Result<(u64, Vec<u8>), WorkerServiceError> {
        let state = Self::runtime()?
            .0
            .attach_grid_snapshot_state(
                SessionId(session_id),
                bmux_session_models::ClientId(uuid::Uuid::nil()),
                &[pane_id],
                usize::MAX,
            )
            .map_err(|error| Self::local_error("snapshot", error))?;
        let snapshot = state
            .snapshots
            .into_iter()
            .find(|snapshot| snapshot.pane_id == pane_id)
            .ok_or_else(|| WorkerServiceError::LocalRuntime {
                reason: "pane snapshot is missing".to_string(),
            })?;
        Ok((snapshot.stream_end, snapshot.encoded))
    }

    fn contains(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> bool {
        Self::runtime().is_ok_and(|runtime| {
            runtime
                .0
                .pane_process_identity(SessionId(session_id), pane_id)
                .is_some()
        })
    }
}

#[must_use]
pub fn local_worker_registry<V>(
    local_node_id: impl Into<String>,
    verifier: V,
) -> WorkerRegistry<LocalPaneRuntime, V>
where
    V: WorkerLeaseVerifier,
{
    WorkerRegistry::new(local_node_id, LocalPaneRuntime, verifier)
}
