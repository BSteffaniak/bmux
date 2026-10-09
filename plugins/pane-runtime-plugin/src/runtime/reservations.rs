//! Durable execution reservation authority. One committed record owns identity.
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v2::{
    CaptureCursor, ExecutionRef, ExitPolicy, LaunchPhase, LaunchReceipt, LaunchReservation,
    ProgramOptions, RecoveryError,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Deserialize, Serialize)]
struct StoredReservation {
    version: u32,
    receipt: LaunchReceipt,
}

static STORE: std::sync::OnceLock<std::sync::Mutex<Option<ReservationStore>>> =
    std::sync::OnceLock::new();

pub(crate) fn activate(context: &bmux_plugin_sdk::NativeLifecycleContext) {
    let store = ReservationStore::open(
        &PathBuf::from(&context.connection.state_dir).join("pane-executions"),
    );
    *STORE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .expect("reservation store lock") = store.ok();
}

pub(crate) fn reserve(
    mutation: uuid::Uuid,
    options: ProgramOptions,
    policy: ExitPolicy,
    authorize: impl Fn(uuid::Uuid) -> Result<(), RecoveryError>,
) -> Result<LaunchReservation, RecoveryError> {
    let store = STORE
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .map_err(failed)?;
    let store = store.as_ref().ok_or_else(|| RecoveryError::Unavailable {
        reason: "execution reservation authority unavailable".into(),
    })?;
    let reservation = store.reserve(mutation, options, policy, authorize)?;
    Ok(reservation)
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
    pub(super) fn reserve(
        &self,
        mutation: uuid::Uuid,
        options: ProgramOptions,
        policy: ExitPolicy,
        authorize: impl Fn(uuid::Uuid) -> Result<(), RecoveryError>,
    ) -> Result<LaunchReservation, RecoveryError> {
        let path = self.root.join(format!("launch-{mutation}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let stored: StoredReservation = serde_json::from_slice(&bytes).map_err(failed)?;
                if stored.version != 1 {
                    return Err(failed("unsupported reservation storage version"));
                }
                let reservation = stored.receipt.reservation;
                authorize(reservation.execution.session_id)?;
                if serde_json::to_vec(&(&reservation.options, &reservation.policy))
                    .map_err(failed)?
                    != serde_json::to_vec(&(&options, &policy)).map_err(failed)?
                {
                    return Err(RecoveryError::Conflict {
                        reason: "conflicting launch mutation identity".into(),
                    });
                }
                Ok(reservation)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let count = std::fs::read_dir(&self.root)
                    .map_err(failed)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(failed)?
                    .iter()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("launch-"))
                    .count();
                if count >= 128 {
                    return Err(RecoveryError::Capacity {
                        reason: "launch reservation capacity exhausted".into(),
                    });
                }
                let execution = ExecutionRef {
                    session_id: uuid::Uuid::new_v4(),
                    pane_id: uuid::Uuid::new_v4(),
                    execution_token: uuid::Uuid::new_v4(),
                };
                authorize(execution.session_id)?;
                let initial_cursor = CaptureCursor {
                    execution: execution.clone(),
                    spool_generation: uuid::Uuid::new_v4(),
                    offset: 0,
                };
                let reservation = LaunchReservation {
                    execution,
                    launch_mutation_id: mutation,
                    revision: 1,
                    options,
                    policy,
                    initial_cursor,
                };
                let receipt = LaunchReceipt {
                    reservation: reservation.clone(),
                    phase: LaunchPhase::Reserved,
                    revision: 1,
                    exit_code: None,
                    exit_signal: None,
                    cleanup_complete: false,
                    error: None,
                };
                crate::atomic_state::commit(
                    &path,
                    &StoredReservation {
                        version: 1,
                        receipt,
                    },
                )
                .map_err(failed)?;
                Ok(reservation)
            }
            Err(error) => Err(failed(error)),
        }
    }
}

fn failed(error: impl std::fmt::Display) -> RecoveryError {
    RecoveryError::OperationFailed {
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reservation_identity_and_initial_cursor_survive_exact_retry() {
        let root = tempfile::tempdir().unwrap();
        let store = super::ReservationStore::open(root.path()).unwrap();
        let mutation = uuid::Uuid::new_v4();
        let options = bmux_pane_runtime_plugin_api::pane_runtime_execution_v2::ProgramOptions {
            program: "/bin/echo".into(),
            args: vec!["literal;$(value)".into()],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
        };
        let first = store.reserve(mutation, options.clone(), bmux_pane_runtime_plugin_api::pane_runtime_execution_v2::ExitPolicy::PreserveDescendants, |_| Ok(())).unwrap();
        let retry = store.reserve(mutation, options, bmux_pane_runtime_plugin_api::pane_runtime_execution_v2::ExitPolicy::PreserveDescendants, |_| Ok(())).unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&retry).unwrap()
        );
        assert_eq!(first.initial_cursor.offset, 0);
        assert_eq!(
            first.initial_cursor.execution.execution_token,
            first.execution.execution_token
        );
    }
}
