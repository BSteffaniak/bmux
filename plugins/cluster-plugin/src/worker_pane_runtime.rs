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

pub struct TypedPaneRuntime {
    caller: Arc<TypedServiceCaller>,
}

impl TypedPaneRuntime {
    #[must_use]
    pub const fn new(caller: Arc<TypedServiceCaller>) -> Self {
        Self { caller }
    }

    fn client(&self) -> bmux_plugin::ServiceCallerDispatchClient<'_, TypedServiceCaller> {
        bmux_plugin::ServiceCallerDispatchClient::new(self.caller.as_ref())
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
        let pane_id = authority.pane_id.value;
        let mut client = self.client();
        let launch = bmux_plugin::block_on_typed_dispatch(
            pane_runtime_commands::client::ensure_execution_pane(
                &mut client,
                session_id,
                pane_id,
                Some(format!("cluster:{pane_id}")),
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
        let mut client = self.client();
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
        let mut client = self.client();
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

    fn output(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cursor: u64,
        max_bytes: u32,
    ) -> Result<bmux_pane_runtime_state::OutputRead, WorkerServiceError> {
        let mut client = self.client();
        let read = bmux_plugin::block_on_typed_dispatch(
            attach_runtime_state::client::pane_output_cursor_state(
                &mut client,
                session_id,
                pane_id,
                cursor,
                max_bytes,
            ),
        )
        .map_err(|error| Self::dispatch_error("output", error))?
        .map_err(|error| Self::service_error("output", error))?;
        Ok(bmux_pane_runtime_state::OutputRead {
            bytes: read.data,
            retained_start: read.retained_start,
            stream_start: read.stream_start,
            stream_end: read.stream_end,
            source_end: read.source_end,
            stream_gap: read.stream_gap,
        })
    }

    fn resize(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
        cols: u16,
        rows: u16,
    ) -> Result<(), WorkerServiceError> {
        let mut client = self.client();
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
        let mut client = self.client();
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
        let mut client = self.client();
        bmux_plugin::block_on_typed_dispatch(pane_runtime_commands::client::destroy_execution_pane(
            &mut client,
            session_id,
            pane_id,
        ))
        .map_err(|error| Self::dispatch_error("close", error))?
        .map(|_| ())
        .map_err(|error| Self::service_error("close", error))
    }

    fn snapshot(
        &self,
        session_id: uuid::Uuid,
        pane_id: uuid::Uuid,
    ) -> Result<(u64, Vec<u8>), WorkerServiceError> {
        let mut client = self.client();
        let snapshot = bmux_plugin::block_on_typed_dispatch(
            attach_runtime_state::client::pane_grid_snapshot_state(
                &mut client,
                session_id,
                pane_id,
                u32::MAX,
            ),
        )
        .map_err(|error| Self::dispatch_error("snapshot", error))?
        .map_err(|error| Self::service_error("snapshot", error))?;
        Ok((snapshot.stream_end, snapshot.encoded))
    }

    fn contains(&self, session_id: uuid::Uuid, pane_id: uuid::Uuid) -> bool {
        let mut client = self.client();
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
) -> WorkerRegistry<TypedPaneRuntime, V>
where
    V: WorkerLeaseVerifier,
{
    WorkerRegistry::new(local_node_id, TypedPaneRuntime::new(caller), verifier)
}
