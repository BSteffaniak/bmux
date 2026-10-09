//! Versioned recovery orchestration over the execution authority's generated API.
use crate::process_recovery_store::{RecoveryRecord, RecoveryStore};
use anyhow::{Context, Result, ensure};
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v2 as pane;
use bmux_plugin::{ServiceCallerDispatchClient, TypedServiceCaller};
use bmux_recording_plugin_api::{
    recording_process_recovery_v2 as recovery, recording_process_v1 as process,
};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

pub(crate) struct RecoveryService {
    caller: Arc<TypedServiceCaller>,
    store: RecoveryStore,
    runtime_root: String,
}

impl RecoveryService {
    pub(crate) fn new(
        caller: Arc<TypedServiceCaller>,
        root: PathBuf,
        runtime_root: String,
    ) -> Result<Self> {
        Ok(Self {
            caller,
            store: RecoveryStore::new(&root)?,
            runtime_root,
        })
    }

    pub(crate) async fn start(
        &self,
        run_id: Uuid,
        launch_mutation_id: Uuid,
        options: process::ProgramOptions,
        runtime_root: String,
    ) -> Result<recovery::RecoveryStatus> {
        ensure!(
            runtime_root == self.runtime_root,
            "isolated runtime locator mismatch"
        );
        let mut record = self.store.admit(&RecoveryRecord {
            status: recovery::RecoveryStatus {
                schema_version: 2,
                run_id,
                revision: 1,
                launch_mutation_id,
                recording_id: None,
                runtime_root,
                reservation: None,
                authority: None,
                cursor: None,
                capture: recovery::CaptureState::Pending,
                recovery_required: true,
                errors: vec![],
            },
            options,
        })?;
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        if record.status.reservation.is_none() {
            let options = &record.options;
            let reservation = pane::client::reserve(
                &mut client,
                launch_mutation_id,
                pane::ProgramOptions {
                    program: options.program.clone(),
                    args: options.args.clone(),
                    cwd: options.cwd.clone(),
                    env: options.env.clone(),
                    cols: options.cols,
                    rows: options.rows,
                },
                pane::ExitPolicy::PreserveDescendants,
            )
            .await?
            .map_err(authority_error)?;
            ensure!(
                reservation.launch_mutation_id == launch_mutation_id
                    && reservation.initial_cursor.offset == 0
                    && serde_json::to_vec(&reservation.execution)?
                        == serde_json::to_vec(&reservation.initial_cursor.execution)?,
                "invalid authoritative reservation identity"
            );
            let expected = record.status.revision;
            record.status.cursor = Some(reservation.initial_cursor.clone());
            record.status.reservation = Some(reservation);
            self.store.replace(expected, &mut record)?;
        }
        // Reconcile first on every retry. Only an authoritative Reserved receipt
        // permits launch; unknown/unreachable/uncertain outcomes never respawn.
        record = self.reconcile_record(record).await?;
        let authority = record
            .status
            .authority
            .as_ref()
            .context("launch authority unavailable")?;
        if matches!(authority.launch.phase, pane::LaunchPhase::Reserved) {
            let reservation = record
                .status
                .reservation
                .clone()
                .context("reservation unavailable")?;
            pane::client::launch(&mut client, reservation)
                .await?
                .map_err(authority_error)?;
            record = self.reconcile_record(record).await?;
        }
        Ok(record.status)
    }

    pub(crate) async fn reconcile(&self, run_id: Uuid) -> Result<recovery::RecoveryStatus> {
        Ok(self
            .reconcile_record(self.store.load(run_id)?)
            .await?
            .status)
    }

    async fn reconcile_record(&self, mut record: RecoveryRecord) -> Result<RecoveryRecord> {
        let execution = record
            .status
            .reservation
            .as_ref()
            .context("launch reservation not yet reconciled; retry original start intent")?
            .execution
            .clone();
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let authority = pane::client::reconcile(&mut client, execution.clone())
            .await?
            .map_err(authority_error)?;
        ensure!(
            authority.launch.reservation.launch_mutation_id == record.status.launch_mutation_id
                && serde_json::to_vec(&authority.launch.reservation.execution)?
                    == serde_json::to_vec(&execution)?,
            "execution authority identity mismatch"
        );
        let expected = record.status.revision;
        record.status.recovery_required = authority.recovery_required
            || matches!(
                record.status.capture,
                recovery::CaptureState::Gap | recovery::CaptureState::Failed
            );
        record.status.authority = Some(authority);
        self.store.replace(expected, &mut record)?;
        Ok(record)
    }

    pub(crate) async fn capture(
        &self,
        run_id: Uuid,
        max_bytes: u32,
    ) -> Result<recovery::RecoveryStatus> {
        ensure!(
            (1..=65_536).contains(&max_bytes),
            "capture read exceeds bound"
        );
        let mut record = self.reconcile_record(self.store.load(run_id)?).await?;
        let cursor = record
            .status
            .cursor
            .clone()
            .context("capture cursor unavailable")?;
        // Recover the exact already committed batch before asking for more bytes.
        // A repeated server read may now have a larger watermark; it must not
        // replace bytes durably accepted before the state cursor commit.
        if let Some(batch) = self.store.committed_batch(run_id, &cursor)? {
            self.store.commit_batch(&mut record, &batch)?;
            return Ok(record.status);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let response = pane::client::capture_resume(&mut client, cursor, max_bytes)
            .await?
            .map_err(authority_error)?;
        match response {
            pane::CaptureResponse::Batch { value } => {
                if !value.data.is_empty() {
                    self.store.commit_batch(&mut record, &value)?;
                }
                let expected = record.status.revision;
                if !matches!(
                    record.status.capture,
                    recovery::CaptureState::Gap
                        | recovery::CaptureState::Finalized
                        | recovery::CaptureState::Failed
                ) {
                    record.status.capture = recovery::CaptureState::Contiguous;
                }
                self.store.replace(expected, &mut record)?;
            }
            pane::CaptureResponse::Gap { value } => {
                let expected = record.status.revision;
                record.status.capture = recovery::CaptureState::Gap;
                record.status.recovery_required = true;
                record
                    .status
                    .errors
                    .push(format!("canonical raw capture gap: {}", value.reason));
                // A screen snapshot cannot reconstruct missing raw evidence.
                self.store.replace(expected, &mut record)?;
            }
        }
        Ok(record.status)
    }

    fn execution(&self, run_id: Uuid) -> Result<pane::ExecutionRef> {
        Ok(self
            .store
            .load(run_id)?
            .status
            .reservation
            .context("reservation unavailable")?
            .execution)
    }

    pub(crate) async fn send(
        &self,
        run_id: Uuid,
        mutation_id: Uuid,
        data: Vec<u8>,
    ) -> Result<pane::InputReceipt> {
        ensure!(data.len() <= 65_536, "input exceeds admission bound");
        let execution = self.execution(run_id)?;
        let request = serde_json::json!({"execution":execution,"data":data});
        if let Some(receipt) =
            self.store
                .admit_mutation(run_id, mutation_id, "input", &request, 1024)?
        {
            return Ok(serde_json::from_value(receipt)?);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let receipt =
            match pane::client::input_receipt(&mut client, execution.clone(), mutation_id).await? {
                Ok(receipt) => receipt,
                Err(pane::RecoveryError::NotFound { .. }) => {
                    // Only authoritative absence permits first admission under the
                    // same identity. Unavailable/uncertain never triggers replay.
                    pane::client::send(&mut client, execution, mutation_id, data)
                        .await?
                        .map_err(authority_error)?
                }
                Err(error) => return Err(authority_error(error)),
            };
        if matches!(
            receipt.phase,
            pane::InputPhase::Completed | pane::InputPhase::Rejected
        ) {
            self.store.complete_mutation(
                run_id,
                mutation_id,
                "input",
                serde_json::to_value(&receipt)?,
            )?;
        }
        Ok(receipt)
    }

    pub(crate) async fn input_receipt(
        &self,
        run_id: Uuid,
        mutation_id: Uuid,
    ) -> Result<pane::InputReceipt> {
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        pane::client::input_receipt(&mut client, self.execution(run_id)?, mutation_id)
            .await?
            .map_err(authority_error)
    }

    pub(crate) async fn resolve(
        &self,
        run_id: Uuid,
        mutation_id: Uuid,
        expected_revision: u64,
        action: pane::ResolutionAction,
        input_mutation_id: Option<Uuid>,
        grace_ms: u32,
    ) -> Result<pane::ResolutionReceipt> {
        let execution = self.execution(run_id)?;
        let request = serde_json::json!({"execution":execution,"expected_revision":expected_revision,
            "action":action,"input_mutation_id":input_mutation_id,"grace_ms":grace_ms});
        if let Some(receipt) =
            self.store
                .admit_mutation(run_id, mutation_id, "resolve", &request, 128)?
        {
            return Ok(serde_json::from_value(receipt)?);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let receipt = pane::client::resolve(
            &mut client,
            execution,
            mutation_id,
            expected_revision,
            action,
            input_mutation_id,
            grace_ms,
        )
        .await?
        .map_err(authority_error)?;
        self.store.complete_mutation(
            run_id,
            mutation_id,
            "resolve",
            serde_json::to_value(&receipt)?,
        )?;
        Ok(receipt)
    }

    pub(crate) async fn retire(
        &self,
        run_id: Uuid,
        mutation_id: Uuid,
        expected_revision: u64,
    ) -> Result<pane::RetirementReceipt> {
        let record = self.store.load(run_id)?;
        RecoveryStore::require_finalizable(&record)?;
        let execution = self.execution(run_id)?;
        let request =
            serde_json::json!({"execution":execution,"expected_revision":expected_revision});
        if let Some(receipt) =
            self.store
                .admit_mutation(run_id, mutation_id, "retire", &request, 128)?
        {
            return Ok(serde_json::from_value(receipt)?);
        }
        let mut client = ServiceCallerDispatchClient::new(self.caller.as_ref());
        let receipt = pane::client::retire(&mut client, execution, mutation_id, expected_revision)
            .await?
            .map_err(authority_error)?;
        self.store.complete_mutation(
            run_id,
            mutation_id,
            "retire",
            serde_json::to_value(&receipt)?,
        )?;
        Ok(receipt)
    }

    pub(crate) async fn finish(&self, run_id: Uuid) -> Result<recovery::RecoveryStatus> {
        let mut record = self.reconcile_record(self.store.load(run_id)?).await?;
        RecoveryStore::require_finalizable(&record)?;
        let expected = record.status.revision;
        record.status.capture = recovery::CaptureState::Finalized;
        self.store.replace(expected, &mut record)?;
        Ok(record.status)
    }
}

fn authority_error(error: pane::RecoveryError) -> anyhow::Error {
    anyhow::anyhow!("execution recovery authority: {error:?}")
}
