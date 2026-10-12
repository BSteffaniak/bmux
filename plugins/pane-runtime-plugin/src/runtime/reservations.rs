//! Durable keyed execution admission. Committed intent precedes process effects.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalCaptureCursor as CaptureCursor, TerminalExecutionRef as ExecutionRef,
    TerminalLaunchPhase as LaunchPhase, TerminalLaunchReceipt as LaunchReceipt,
    TerminalLaunchReservation as LaunchReservation, TerminalProgramOptions as ProgramOptions,
    TerminalRecoveryError as RecoveryError,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Deserialize, Serialize)]
struct StoredReservation {
    version: u32,
    receipt: LaunchReceipt,
}
static STORE: OnceLock<Mutex<Result<ReservationStore, String>>> = OnceLock::new();
pub(crate) fn activate(context: &bmux_plugin_sdk::NativeLifecycleContext) {
    let opened = ReservationStore::open(
        &PathBuf::from(&context.connection.state_dir).join("pane-executions-v4"),
    )
    .map_err(|error| format!("{error:?}"));
    let _ = STORE.set(Mutex::new(opened));
}
fn with_store<T>(
    action: impl FnOnce(&ReservationStore) -> Result<T, RecoveryError>,
) -> Result<T, RecoveryError> {
    let store = STORE
        .get()
        .ok_or_else(|| RecoveryError::Unavailable {
            reason: "execution authority not activated".into(),
        })?
        .lock()
        .map_err(failed)?;
    let store = store
        .as_ref()
        .map_err(|reason| RecoveryError::RecoveryRequired {
            reason: reason.clone(),
        })?;
    action(store)
}
pub(crate) fn reserve(
    mutation: uuid::Uuid,
    options: ProgramOptions,
    authorize: impl Fn(uuid::Uuid) -> Result<(), RecoveryError>,
) -> Result<LaunchReservation, RecoveryError> {
    with_store(|store| store.reserve(mutation, options, authorize))
}
pub(crate) fn launch(
    reservation: &LaunchReservation,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<LaunchReceipt, RecoveryError> {
    launch_named(reservation, None, authorize)
}
pub(super) fn launch_named(
    reservation: &LaunchReservation,
    name: Option<String>,
    authorize: impl Fn(uuid::Uuid) -> anyhow::Result<()> + Send + Sync + 'static,
) -> Result<LaunchReceipt, RecoveryError> {
    // Drop storage mutex before awaiting runtime work: capture callbacks must
    // be able to persist progress while launch waits for startup publication.
    let admitted = with_store(|store| {
        let mut stored = store.read(reservation.launch_mutation_id)?;
        authorize(stored.receipt.reservation.execution.session_id).map_err(|error| {
            RecoveryError::Denied {
                reason: error.to_string(),
            }
        })?;
        if serde_json::to_vec(&reservation).map_err(failed)?
            != serde_json::to_vec(&stored.receipt.reservation).map_err(failed)?
        {
            return Err(RecoveryError::Conflict {
                reason: "reservation does not match committed admission".into(),
            });
        }
        if !matches!(stored.receipt.phase, LaunchPhase::Reserved) {
            return Ok((false, stored.receipt));
        }
        stored.receipt.phase = LaunchPhase::Launching;
        stored.receipt.revision += 1;
        store.commit(&stored)?;
        Ok((true, stored.receipt))
    })?;
    if !admitted.0 {
        if matches!(
            admitted.1.phase,
            LaunchPhase::Launching | LaunchPhase::Uncertain
        ) {
            return Err(RecoveryError::RecoveryRequired {
                reason:
                    "launch intent already committed; reconcile retained identity, never relaunch"
                        .into(),
            });
        }
        return Ok(admitted.1);
    }
    let result = super::execution::create_reserved(
        name,
        reservation.options.clone(),
        Some(&reservation.execution),
        authorize,
    );
    with_store(|store| {
        let mut stored = store.read(reservation.launch_mutation_id)?;
        stored.receipt.revision += 1;
        match result {
            Ok(reference) if reference == reservation.execution => {
                stored.receipt.phase = LaunchPhase::Running;
            }
            Ok(_) => {
                stored.receipt.phase = LaunchPhase::Uncertain;
                stored.receipt.error = Some("runtime identity differs from admission".into());
            }
            Err(error) => {
                stored.receipt.phase = LaunchPhase::Uncertain;
                stored.receipt.error =
                    Some(format!("launch effect requires reconciliation: {error:?}"));
            }
        }
        store.commit(&stored)?;
        Ok(stored.receipt)
    })
}

pub(super) struct ReservationStore {
    root: PathBuf,
}
impl ReservationStore {
    pub(super) fn open(root: &Path) -> Result<Self, RecoveryError> {
        std::fs::create_dir_all(root).map_err(failed)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(failed)?;
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }
    fn read(&self, mutation: uuid::Uuid) -> Result<StoredReservation, RecoveryError> {
        let bytes =
            std::fs::read(self.root.join(format!("launch-{mutation}.json"))).map_err(failed)?;
        let stored: StoredReservation = serde_json::from_slice(&bytes).map_err(failed)?;
        if stored.version != 4 {
            return Err(RecoveryError::RecoveryRequired {
                reason: "unsupported execution authority format".into(),
            });
        }
        Ok(stored)
    }
    fn commit(&self, stored: &StoredReservation) -> Result<(), RecoveryError> {
        crate::atomic_state::commit(
            &self.root.join(format!(
                "launch-{}.json",
                stored.receipt.reservation.launch_mutation_id
            )),
            stored,
        )
        .map_err(failed)
    }
    pub(super) fn reserve(
        &self,
        mutation: uuid::Uuid,
        options: ProgramOptions,
        authorize: impl Fn(uuid::Uuid) -> Result<(), RecoveryError>,
    ) -> Result<LaunchReservation, RecoveryError> {
        let path = self.root.join(format!("launch-{mutation}.json"));
        if path.exists() {
            let stored = self.read(mutation)?;
            authorize(stored.receipt.reservation.execution.session_id)?;
            if serde_json::to_vec(&options).map_err(failed)?
                != serde_json::to_vec(&stored.receipt.reservation.options).map_err(failed)?
            {
                return Err(RecoveryError::Conflict {
                    reason: "conflicting launch mutation identity".into(),
                });
            }
            return Ok(stored.receipt.reservation);
        }
        if std::fs::read_dir(&self.root)
            .map_err(failed)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(failed)?
            .into_iter()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("launch-"))
            .take(128)
            .count()
            >= 128
        {
            return Err(RecoveryError::Capacity {
                reason: "execution authority capacity exhausted".into(),
            });
        }
        super::execution::validate_options(&options).map_err(|error| {
            RecoveryError::InvalidOptions {
                reason: format!("{error:?}"),
            }
        })?;
        let execution = ExecutionRef {
            session_id: uuid::Uuid::new_v4(),
            pane_id: uuid::Uuid::new_v4(),
            execution_token: uuid::Uuid::new_v4(),
        };
        authorize(execution.session_id)?;
        let reservation = LaunchReservation {
            initial_cursor: CaptureCursor {
                execution: execution.clone(),
                spool_generation: uuid::Uuid::new_v4(),
                offset: 0,
            },
            execution,
            launch_mutation_id: mutation,
            revision: 1,
            options,
        };
        self.commit(&StoredReservation {
            version: 4,
            receipt: LaunchReceipt {
                reservation: reservation.clone(),
                phase: LaunchPhase::Reserved,
                revision: 1,
                exit_code: None,
                exit_signal: None,
                cleanup_complete: false,
                error: None,
            },
        })?;
        Ok(reservation)
    }
}
fn failed(error: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::OperationFailed {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_admission_checks_running_exact_token_recovery_and_end() {
        use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
            TerminalCaptureIntegrity, TerminalExecutionPhase, TerminalExecutionStatus,
        };
        let root = tempfile::tempdir().unwrap();
        let store = ReservationStore::open(root.path()).unwrap();
        let reservation = store
            .reserve(
                uuid::Uuid::new_v4(),
                ProgramOptions {
                    program: "/bin/echo".into(),
                    args: vec![],
                    cwd: None,
                    env: std::collections::BTreeMap::new(),
                    cols: 80,
                    rows: 24,
                },
                |_| Ok(()),
            )
            .unwrap();
        let reference = reservation.execution.clone();
        let mut launch = store.read(reservation.launch_mutation_id).unwrap().receipt;
        launch.phase = LaunchPhase::Running;
        let mut state = TerminalExecutionStatus {
            execution: reference.clone(), phase: TerminalExecutionPhase::Running,
            lifecycle: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalLifecycle::ConventionalTerminalSession,
            session_end: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalSessionEnd::Active,
            terminal_session_ended: false, capture_integrity: TerminalCaptureIntegrity::Capturing,
            exit_code: None, signal: None, output_drained: false, output_watermark: 0, cleanup_complete: false, error: None,
        };
        assert!(validate_input_authority(&reference, &launch, &state, false).is_ok());
        assert!(validate_input_authority(&reference, &launch, &state, true).is_err());
        state.execution.execution_token = uuid::Uuid::new_v4();
        assert!(validate_input_authority(&reference, &launch, &state, false).is_err());
        state.execution = reference.clone();
        state.terminal_session_ended = true;
        assert!(validate_input_authority(&reference, &launch, &state, false).is_err());
        state.terminal_session_ended = false;
        state.phase = TerminalExecutionPhase::Exited;
        assert!(validate_input_authority(&reference, &launch, &state, false).is_err());
        state.phase = TerminalExecutionPhase::Running;
        launch.phase = LaunchPhase::Uncertain;
        assert!(validate_input_authority(&reference, &launch, &state, false).is_err());
        launch.phase = LaunchPhase::Running;
        state.error = Some("capture failed".into());
        assert!(validate_input_authority(&reference, &launch, &state, false).is_err());
    }

    #[test]
    fn keyed_admission_preserves_identity_rejects_conflict_and_corruption() {
        let root = tempfile::tempdir().unwrap();
        let store = ReservationStore::open(root.path()).unwrap();
        let mutation = uuid::Uuid::new_v4();
        let options = ProgramOptions {
            program: "/bin/echo".into(),
            args: vec!["literal".into()],
            env: std::collections::BTreeMap::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        };
        let first = store
            .reserve(mutation, options.clone(), |_| Ok(()))
            .unwrap();
        let reopened = ReservationStore::open(root.path()).unwrap();
        assert_eq!(
            first,
            reopened
                .reserve(mutation, options.clone(), |_| Ok(()))
                .unwrap()
        );
        let mut conflict = options;
        conflict.args.push("different".into());
        assert!(matches!(
            store.reserve(mutation, conflict, |_| Ok(())),
            Err(RecoveryError::Conflict { .. })
        ));
        let path = root.path().join(format!("launch-{mutation}.json"));
        std::fs::write(path, b"corrupt").unwrap();
        assert!(store.read(mutation).is_err());
    }
}

pub(super) fn create_capture(
    reference: &ExecutionRef,
) -> Result<super::durable_capture::DurableCapture, RecoveryError> {
    with_store(|store| {
        super::durable_capture::DurableCapture::create(
            &store
                .root
                .join(format!("capture-{}", reference.execution_token)),
        )
        .map_err(failed)
    })
}

fn find(
    store: &ReservationStore,
    reference: &ExecutionRef,
) -> Result<StoredReservation, RecoveryError> {
    for entry in std::fs::read_dir(&store.root).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("launch-") || !name.ends_with(".json") {
            continue;
        }
        let mutation = name
            .trim_start_matches("launch-")
            .trim_end_matches(".json")
            .parse::<uuid::Uuid>()
            .map_err(failed)?;
        let stored = store.read(mutation)?;
        if stored.receipt.reservation.execution == *reference {
            return Ok(stored);
        }
    }
    Err(RecoveryError::NotFound {
        reason: "execution has no durable admission".into(),
    })
}
pub(crate) fn reconcile(
    reference: &ExecutionRef,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalReconciliation,
    RecoveryError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalCaptureIntegrity, TerminalReconciliation, TerminalSessionEnd,
    };
    let current = super::execution::status(reference);
    with_store(|store| {
        let mut stored = find(store, reference)?;
        let root = store
            .root
            .join(format!("capture-{}", reference.execution_token));
        let boundary = if root.exists() {
            Some(
                super::durable_capture::DurableCapture::read(&root, 0, 1)
                    .map_err(failed)?
                    .0,
            )
        } else {
            None
        };
        let mut ended = false;
        let mut recovery = matches!(
            stored.receipt.phase,
            LaunchPhase::Launching | LaunchPhase::Uncertain
        );
        if let Ok(status) = current {
            let previous = serde_json::to_vec(&stored.receipt).map_err(failed)?;
            ended = status.terminal_session_ended;
            stored.receipt.exit_code = status.exit_code;
            stored.receipt.exit_signal = status.signal;
            stored.receipt.cleanup_complete = status.cleanup_complete;
            stored.receipt.error = status.error;
            if ended {
                stored.receipt.phase = LaunchPhase::Exited;
            }
            recovery = stored.receipt.error.is_some();
            if serde_json::to_vec(&stored.receipt).map_err(failed)? != previous {
                stored.receipt.revision += 1;
                store.commit(&stored)?;
            }
        } else if !matches!(
            stored.receipt.phase,
            LaunchPhase::Reserved | LaunchPhase::Retired | LaunchPhase::Exited
        ) {
            recovery = true;
        } else {
            ended = matches!(
                stored.receipt.phase,
                LaunchPhase::Exited | LaunchPhase::Retired
            );
        }
        let complete = boundary
            .as_ref()
            .is_some_and(|boundary| boundary.eof && boundary.error.is_none());
        recovery |= boundary
            .as_ref()
            .is_some_and(|boundary| boundary.error.is_some());
        let oldest = stored.receipt.reservation.initial_cursor.clone();
        let mut continuation = oldest.clone();
        continuation.offset = boundary.as_ref().map_or(0, |boundary| boundary.bytes);
        Ok(TerminalReconciliation {
            launch: stored.receipt,
            supervisor: None,
            capture_complete: complete,
            oldest_cursor: oldest,
            continuation_cursor: continuation,
            recovery_required: recovery,
            session_end: if recovery {
                TerminalSessionEnd::RecoveryRequired
            } else if ended {
                TerminalSessionEnd::Ended
            } else {
                TerminalSessionEnd::Active
            },
            terminal_session_ended: ended,
            capture_integrity: if complete {
                TerminalCaptureIntegrity::Complete
            } else if recovery {
                TerminalCaptureIntegrity::Incomplete
            } else {
                TerminalCaptureIntegrity::Capturing
            },
            // Original recording event timestamps are not stored in the raw spool.
            timing_complete: false,
        })
    })
}
pub(crate) fn capture(
    cursor: CaptureCursor,
    max_bytes: u32,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalCaptureResponse,
    RecoveryError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalCaptureBatch, TerminalCaptureGap, TerminalCaptureResponse,
    };
    with_store(|store| {
        let stored = find(store, &cursor.execution)?;
        let oldest = stored.receipt.reservation.initial_cursor;
        if cursor.spool_generation != oldest.spool_generation {
            return Ok(TerminalCaptureResponse::Gap {
                value: TerminalCaptureGap {
                    requested: cursor,
                    oldest: oldest.clone(),
                    continuation: oldest,
                    reason: "capture generation mismatch; snapshot repair unavailable".into(),
                    encoded_grid: None,
                    snapshot_cursor: None,
                },
            });
        }
        let root = store
            .root
            .join(format!("capture-{}", cursor.execution.execution_token));
        let (boundary, bytes) =
            super::durable_capture::DurableCapture::read(&root, cursor.offset, max_bytes)
                .map_err(failed)?;
        let mut continuation = cursor.clone();
        continuation.offset += u64::try_from(bytes.len()).map_err(failed)?;
        Ok(TerminalCaptureResponse::Batch {
            value: TerminalCaptureBatch {
                complete: continuation.offset == boundary.bytes,
                cursor,
                continuation,
                data: bytes,
            },
        })
    })
}

pub(crate) fn capture_events(
    reference: &ExecutionRef,
    _format: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalCaptureFormat,
    offset: u64,
    max_bytes: u32,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalEventBatch,
    RecoveryError,
> {
    with_store(|store| {
        find(store, reference)?;
        let root = store
            .root
            .join(format!("capture-{}", reference.execution_token));
        let (boundary, data) =
            super::durable_capture::DurableCapture::read_events(&root, offset, max_bytes)
                .map_err(failed)?;
        if let Some(error) = boundary.error {
            return Err(failed(error));
        }
        let continuation = offset + u64::try_from(data.len()).map_err(failed)?;
        Ok(bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalEventBatch {
            execution: reference.clone(),
            format: bmux_pane_runtime_plugin_api::pane_runtime_presentation_v1::TerminalCaptureFormat::OrderedEventsV1,
            offset, continuation, data,
            eof: boundary.eof && continuation == boundary.stream_bytes,
        })
    })
}

#[derive(Serialize, Deserialize)]
struct StoredInput {
    version: u32,
    data: Vec<u8>,
    receipt: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalInputReceipt,
}
pub(crate) fn input_receipt(
    reference: &ExecutionRef,
    mutation: uuid::Uuid,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalInputReceipt,
    RecoveryError,
> {
    with_store(|store| {
        find(store, reference)?;
        let bytes = std::fs::read(store.root.join(format!(
            "input-{}-{mutation}.json",
            reference.execution_token
        )))
        .map_err(failed)?;
        let stored: StoredInput = serde_json::from_slice(&bytes).map_err(failed)?;
        if stored.version != 4 || stored.receipt.execution != *reference {
            return Err(RecoveryError::RecoveryRequired {
                reason: "unsupported or mismatched input receipt".into(),
            });
        }
        Ok(stored.receipt)
    })
}
fn validate_input_authority(
    reference: &ExecutionRef,
    launch: &LaunchReceipt,
    state: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionStatus,
    capture_error: bool,
) -> Result<(), RecoveryError> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalCaptureIntegrity, TerminalExecutionPhase,
    };
    if launch.reservation.execution != *reference || state.execution != *reference {
        return Err(RecoveryError::RecoveryRequired {
            reason: "stale execution input authority".into(),
        });
    }
    if !matches!(launch.phase, LaunchPhase::Running)
        || !matches!(state.phase, TerminalExecutionPhase::Running)
        || state.terminal_session_ended
        || launch.error.is_some()
        || state.error.is_some()
        || matches!(
            state.capture_integrity,
            TerminalCaptureIntegrity::Incomplete
        )
        || capture_error
    {
        return Err(RecoveryError::RecoveryRequired {
            reason: "execution authority is not currently writable; reconcile/resolve before input"
                .into(),
        });
    }
    Ok(())
}

fn validate_fresh_input(
    store: &ReservationStore,
    reference: &ExecutionRef,
) -> Result<(), RecoveryError> {
    // Move recording's pre-send reconcile admission here, nearer
    // the effect. Read identical launch/runtime/capture authority
    // without mutating reconciliation state. Existing receipts
    // still replay unchanged before this fresh-effect check.
    let launch = find(store, reference)?;
    let state =
        super::execution::status(reference).map_err(|error| RecoveryError::RecoveryRequired {
            reason: format!("input authority unavailable: {error:?}"),
        })?;
    let capture_root = store
        .root
        .join(format!("capture-{}", reference.execution_token));
    let capture_error = if capture_root.exists() {
        super::durable_capture::DurableCapture::read(&capture_root, 0, 1)
            .map_err(failed)?
            .0
            .error
            .is_some()
    } else {
        false
    };
    validate_input_authority(reference, &launch.receipt, &state, capture_error)?;
    Ok(())
}

pub(crate) fn send_admitted(
    reference: &ExecutionRef,
    mutation: uuid::Uuid,
    data: Vec<u8>,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalInputReceipt,
    RecoveryError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalInputPhase, TerminalInputReceipt,
    };
    if data.len() > 65536 {
        return Err(RecoveryError::InvalidOptions {
            reason: "input exceeds 65536 bytes".into(),
        });
    }
    let claim = with_store(|store| {
        find(store, reference)?;
        let path = store.root.join(format!(
            "input-{}-{mutation}.json",
            reference.execution_token
        ));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let stored: StoredInput = serde_json::from_slice(&bytes).map_err(failed)?;
                if stored.version != 4 || stored.receipt.execution != *reference {
                    return Err(RecoveryError::RecoveryRequired {
                        reason: "input receipt unsupported".into(),
                    });
                }
                if stored.data != data {
                    return Err(RecoveryError::Conflict {
                        reason: "conflicting input mutation".into(),
                    });
                }
                Ok((false, stored))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                validate_fresh_input(store, reference)?;
                let count = std::fs::read_dir(&store.root)
                    .map_err(failed)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(failed)?
                    .into_iter()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("input-"))
                    .take(4096)
                    .count();
                if count >= 4096 {
                    return Err(RecoveryError::Capacity {
                        reason: "retained input receipt capacity exhausted".into(),
                    });
                }
                let stored = StoredInput {
                    version: 4,
                    data: data.clone(),
                    receipt: TerminalInputReceipt {
                        execution: reference.clone(),
                        input_mutation_id: mutation,
                        revision: 1,
                        phase: TerminalInputPhase::Writing,
                        admitted_bytes: u64::try_from(data.len()).map_err(failed)?,
                        acknowledged_bytes: 0,
                        error: None,
                    },
                };
                crate::atomic_state::commit(&path, &stored).map_err(failed)?;
                Ok((true, stored))
            }
            Err(error) => Err(failed(error)),
        }
    })?;
    if !claim.0 {
        return Ok(claim.1.receipt);
    }
    let result = super::execution::send_acknowledged(reference, data);
    with_store(|store| {
        let mut stored = claim.1;
        stored.receipt.revision += 1;
        match result {
            Ok(()) => {
                stored.receipt.phase = TerminalInputPhase::Completed;
                stored.receipt.acknowledged_bytes = stored.receipt.admitted_bytes;
            }
            Err(error) => {
                stored.receipt.phase = TerminalInputPhase::Uncertain;
                stored.receipt.error = Some(format!("{error:?}"));
            }
        }
        crate::atomic_state::commit(
            &store.root.join(format!(
                "input-{}-{mutation}.json",
                reference.execution_token
            )),
            &stored,
        )
        .map_err(failed)?;
        Ok(stored.receipt)
    })
}

pub(crate) fn retire(
    reference: &ExecutionRef,
    mutation: uuid::Uuid,
    expected: u64,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalRetirementReceipt,
    RecoveryError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalInputPhase, TerminalRetirementReceipt,
    };
    with_store(|store| {
        let path = store.root.join(format!(
            "retire-{}-{mutation}.json",
            reference.execution_token
        ));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let retained: (u32, u64, TerminalRetirementReceipt) =
                    serde_json::from_slice(&bytes).map_err(failed)?;
                if retained.0 != 4 || retained.1 != expected || retained.2.execution != *reference {
                    return Err(RecoveryError::Conflict {
                        reason: "conflicting retirement mutation".into(),
                    });
                }
                let mut stored = find(store, reference)?;
                if stored.receipt.revision == expected {
                    stored.receipt.phase = LaunchPhase::Retired;
                    stored.receipt.revision = retained.2.revision;
                    store.commit(&stored)?;
                } else if stored.receipt.revision != retained.2.revision
                    || !matches!(stored.receipt.phase, LaunchPhase::Retired)
                {
                    return Err(RecoveryError::RecoveryRequired {
                        reason: "retirement intent conflicts with retained authority".into(),
                    });
                }
                return Ok(retained.2);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(failed(error)),
        }
        let mut stored = find(store, reference)?;
        if stored.receipt.revision != expected {
            return Err(RecoveryError::RevisionConflict {
                actual_revision: stored.receipt.revision,
            });
        }
        let root = store
            .root
            .join(format!("capture-{}", reference.execution_token));
        let boundary = super::durable_capture::DurableCapture::read(&root, 0, 1)
            .map_err(failed)?
            .0;
        if !boundary.eof
            || boundary.error.is_some()
            || !matches!(stored.receipt.phase, LaunchPhase::Exited)
            || stored.receipt.error.is_some()
        {
            return Err(RecoveryError::RecoveryRequired {
                reason: "retirement requires retained terminal end and durable complete capture"
                    .into(),
            });
        }
        for entry in std::fs::read_dir(&store.root).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("input-{}-", reference.execution_token))
            {
                continue;
            }
            let input: StoredInput =
                serde_json::from_slice(&std::fs::read(entry.path()).map_err(failed)?)
                    .map_err(failed)?;
            if input.version != 4
                || matches!(
                    input.receipt.phase,
                    TerminalInputPhase::Writing
                        | TerminalInputPhase::Accepted
                        | TerminalInputPhase::Uncertain
                )
            {
                return Err(RecoveryError::RecoveryRequired {
                    reason: "unresolved input receipts prevent retirement".into(),
                });
            }
        }
        // Commit a durable retirement intent before changing the launch receipt.
        // Retry can finish this transition; no canonical spool/evidence is deleted.
        let receipt = TerminalRetirementReceipt {
            execution: reference.clone(),
            retirement_mutation_id: mutation,
            revision: expected + 1,
            retired: true,
        };
        crate::atomic_state::commit(&path, &(4_u32, expected, &receipt)).map_err(failed)?;
        stored.receipt.phase = LaunchPhase::Retired;
        stored.receipt.revision = receipt.revision;
        store.commit(&stored)?;
        Ok(receipt)
    })
}

pub(crate) fn resolve(
    reference: &ExecutionRef,
    mutation: uuid::Uuid,
    expected: u64,
    action: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalResolutionAction,
    input: Option<uuid::Uuid>,
    grace: u32,
    ctx: &bmux_plugin_sdk::NativeServiceContext,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalResolutionReceipt,
    RecoveryError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalResolutionAction, TerminalResolutionReceipt,
    };
    let request = serde_json::to_vec(&(reference, mutation, expected, &action, input, grace))
        .map_err(failed)?;
    let claim = with_store(|store| {
        let path = store.root.join(format!(
            "resolve-{}-{mutation}.json",
            reference.execution_token
        ));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let retained: (u32, Vec<u8>, Option<TerminalResolutionReceipt>) =
                    serde_json::from_slice(&bytes).map_err(failed)?;
                if retained.0 != 4 || retained.1 != request {
                    return Err(RecoveryError::Conflict {
                        reason: "conflicting resolution mutation".into(),
                    });
                }
                retained
                    .2
                    .map(|receipt| (false, Some(receipt)))
                    .ok_or_else(|| RecoveryError::RecoveryRequired {
                        reason: "resolution intent unsettled; no effect replay".into(),
                    })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let stored = find(store, reference)?;
                if stored.receipt.revision != expected {
                    return Err(RecoveryError::RevisionConflict {
                        actual_revision: stored.receipt.revision,
                    });
                }
                if matches!(action, TerminalResolutionAction::CancelUnstarted)
                    && !matches!(stored.receipt.phase, LaunchPhase::Reserved)
                {
                    return Err(RecoveryError::RecoveryRequired {
                        reason: "launch intent already exists; cannot cancel as unstarted".into(),
                    });
                }
                crate::atomic_state::commit(
                    &path,
                    &(4_u32, &request, None::<TerminalResolutionReceipt>),
                )
                .map_err(failed)?;
                Ok((true, None))
            }
            Err(error) => Err(failed(error)),
        }
    })?;
    if let Some(receipt) = claim.1 {
        return Ok(receipt);
    }
    let effect = match &action {
        TerminalResolutionAction::TerminateOwnedExecution => {
            super::execution::terminate(reference, grace, || {
                crate::handlers::execution::authorize(ctx, reference, "pane.close")
            })
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
        }
        TerminalResolutionAction::CancelUnstarted => Ok(()),
        TerminalResolutionAction::AcknowledgeUncertainInput => {
            if input.is_none() {
                Err("input mutation is required".into())
            } else {
                Ok(())
            }
        }
    };
    settle_resolution(
        reference, mutation, expected, action, input, request, effect,
    )
}

fn settle_resolution(
    reference: &ExecutionRef,
    mutation: uuid::Uuid,
    expected: u64,
    action: bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalResolutionAction,
    input: Option<uuid::Uuid>,
    request: Vec<u8>,
    effect: Result<(), String>,
) -> Result<
    bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalResolutionReceipt,
    RecoveryError,
> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalInputPhase, TerminalResolutionAction, TerminalResolutionReceipt,
    };
    with_store(|store| {
        let mut stored = find(store, reference)?;
        if stored.receipt.revision != expected {
            return Err(RecoveryError::RevisionConflict {
                actual_revision: stored.receipt.revision,
            });
        }
        if effect.is_ok() {
            match action {
                TerminalResolutionAction::CancelUnstarted => {
                    stored.receipt.phase = LaunchPhase::Failed;
                    stored.receipt.error = Some("unstarted launch explicitly cancelled".into());
                }
                TerminalResolutionAction::AcknowledgeUncertainInput => {
                    let id = input.ok_or_else(|| RecoveryError::InvalidOptions {
                        reason: "input mutation required".into(),
                    })?;
                    let path = store
                        .root
                        .join(format!("input-{}-{id}.json", reference.execution_token));
                    let mut value: StoredInput =
                        serde_json::from_slice(&std::fs::read(&path).map_err(failed)?)
                            .map_err(failed)?;
                    if value.version != 4 || value.receipt.execution != *reference {
                        return Err(RecoveryError::RecoveryRequired {
                            reason: "input authority mismatch".into(),
                        });
                    }
                    if !matches!(
                        value.receipt.phase,
                        TerminalInputPhase::Writing | TerminalInputPhase::Uncertain
                    ) {
                        return Err(RecoveryError::Conflict {
                            reason: "input is not uncertain".into(),
                        });
                    }
                    value.receipt.phase = TerminalInputPhase::Rejected;
                    value.receipt.revision += 1;
                    value.receipt.error = Some(
                        "uncertainty explicitly acknowledged; consumption not asserted".into(),
                    );
                    crate::atomic_state::commit(&path, &value).map_err(failed)?;
                }
                TerminalResolutionAction::TerminateOwnedExecution => {}
            }
        }
        stored.receipt.revision += 1;
        store.commit(&stored)?;
        let receipt = TerminalResolutionReceipt {
            execution: reference.clone(),
            resolution_mutation_id: mutation,
            revision: stored.receipt.revision,
            action,
            input_mutation_id: input,
            cleanup_complete: stored.receipt.cleanup_complete,
            recovery_required: effect.is_err(),
            error: effect.err(),
        };
        crate::atomic_state::commit(
            &store.root.join(format!(
                "resolve-{}-{mutation}.json",
                reference.execution_token
            )),
            &(4_u32, request, Some(&receipt)),
        )
        .map_err(failed)?;
        Ok(receipt)
    })
}
