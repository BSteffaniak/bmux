//! In-server ownership of each pane's terminal session.
//!
//! The pane's program is spawned directly on the PTY and becomes the leader of
//! its own session (Unix) or the only member of its own kill-on-close Job Object
//! (Windows). [`SessionControl`] holds that authority for the pane's lifetime:
//! cooperative and forced stops, the post-exit sweep, and release.
//!
//! On Unix each live session also leaves a crash record in
//! `<runtime_dir>/pane-sessions/`. If the server dies without releasing a
//! session (SIGKILL, OOM), the next activation for the same runtime sweeps the
//! recorded sessions before admitting new panes; see [`recover`].
use std::path::{Path, PathBuf};
use switchy_thread::sync::Mutex;
use switchy_thread::sync::atomic::{AtomicBool, Ordering};

#[cfg(unix)]
#[path = "session_control/unix.rs"]
mod backend;
#[cfg(windows)]
#[path = "session_control/windows.rs"]
mod backend;

pub(super) use backend::Preparation;

/// Directory name for crash records inside the runtime directory.
const RECORDS: &str = "pane-sessions";

/// One activation's session settings and crash-record root.
#[derive(Debug)]
pub(crate) struct SessionAuthority {
    records: Mutex<Option<PathBuf>>,
    sweep_on_exit: AtomicBool,
}

impl Default for SessionAuthority {
    fn default() -> Self {
        Self {
            records: Mutex::new(None),
            sweep_on_exit: AtomicBool::new(true),
        }
    }
}

impl SessionAuthority {
    /// Whether ordinary panes sweep leftover jobs after their shell exits.
    /// Program executions always sweep.
    pub(crate) fn sweep_on_exit(&self) -> bool {
        self.sweep_on_exit.load(Ordering::SeqCst)
    }

    fn records(&self) -> Option<PathBuf> {
        self.records.lock().ok().and_then(|root| root.clone())
    }
}

/// Plugin settings under `[plugins.settings."bmux.pane_runtime".execution]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecutionSettings {
    pub(crate) sweep_on_exit: bool,
}

impl Default for ExecutionSettings {
    fn default() -> Self {
        Self {
            sweep_on_exit: true,
        }
    }
}

impl ExecutionSettings {
    /// Parse the `execution` table of the plugin's settings.
    ///
    /// # Errors
    /// Unknown keys or values of the wrong type.
    pub(crate) fn parse(settings: Option<&toml::Value>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let Some(table) = settings.and_then(|settings| settings.get("execution")) else {
            return Ok(parsed);
        };
        let table = table
            .as_table()
            .ok_or("pane runtime `execution` settings must be a table")?;
        for (key, value) in table {
            match key.as_str() {
                "sweep_on_exit" => {
                    parsed.sweep_on_exit = value
                        .as_bool()
                        .ok_or("pane runtime `execution.sweep_on_exit` must be a boolean")?;
                }
                other => {
                    return Err(format!(
                        "unknown pane runtime `execution` setting `{other}`"
                    ));
                }
            }
        }
        Ok(parsed)
    }
}

/// Install this activation's settings, then sweep sessions left by a crashed
/// predecessor of the same runtime before any pane is admitted.
pub(crate) fn activate(
    host: &crate::runtime::PaneRuntimeHost,
    context: &bmux_plugin_sdk::NativeLifecycleContext,
    settings: ExecutionSettings,
) {
    let authority = host.sessions();
    authority
        .sweep_on_exit
        .store(settings.sweep_on_exit, Ordering::SeqCst);
    let runtime_dir = Path::new(&context.connection.runtime_dir);
    if runtime_dir.is_absolute() {
        let records = runtime_dir.join(RECORDS);
        if let Ok(mut root) = authority.records.lock() {
            *root = Some(records.clone());
        }
        recover(&records);
        remove_legacy_supervisor_state(runtime_dir, Path::new(&context.connection.state_dir));
    }
}

/// Sweep every session recorded by a predecessor, then delete its record.
pub(crate) fn recover(records: &Path) {
    let Ok(entries) = std::fs::read_dir(records) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            // Interrupted atomic-commit staging files.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        match backend::recover_record(&path) {
            Ok(()) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(error) => {
                tracing::warn!(
                    record = %path.display(),
                    %error,
                    "could not sweep a pane session left by a previous server; keeping its record"
                );
            }
        }
    }
}

/// The out-of-process supervisor this backend replaced left locators, private
/// endpoint directories and retained helper images behind. A new server for the
/// same runtime means the old server, and with it every guardian bound to its
/// lifetime, is gone, so this state is inert and safe to delete.
fn remove_legacy_supervisor_state(runtime_dir: &Path, state_dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(runtime_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("supervisor-") && name.ends_with(".json") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if state_dir.is_absolute() {
        let _ = std::fs::remove_dir_all(state_dir.join("auxiliary-images"));
    }
}

/// Authority over one pane's terminal session.
#[derive(Debug)]
pub(crate) struct SessionControl {
    backend: backend::Session,
    leader: u32,
    /// Set, under the execution's session lock, once the leader has exited
    /// and before it is reaped. Signals to the leader pid are refused after.
    leader_exited: AtomicBool,
    record: Mutex<Option<PathBuf>>,
}

impl SessionControl {
    /// Platform preparation that must exist before the spawn (Windows: the job).
    ///
    /// # Errors
    /// Native resource creation failures.
    pub(super) fn prepare() -> anyhow::Result<Preparation> {
        backend::prepare()
    }

    /// Take ownership of a freshly spawned leader.
    ///
    /// # Errors
    /// A crash record that cannot be committed. Without it a crash could leave
    /// HUP-ignoring jobs behind with nothing to find them.
    pub(super) fn adopt(
        host: &crate::runtime::PaneRuntimeHost,
        preparation: Preparation,
        token: uuid::Uuid,
        leader: u32,
        master: &bmux_pty::PtyMaster,
    ) -> anyhow::Result<Self> {
        let backend = backend::Session::adopt(preparation, leader, master)?;
        let record = match host.sessions().records() {
            Some(root) => backend.commit_record(&root, token)?,
            None => None,
        };
        Ok(Self {
            backend,
            leader,
            leader_exited: AtomicBool::new(false),
            record: Mutex::new(record),
        })
    }

    pub(super) fn leader_exited(&self) -> bool {
        self.leader_exited.load(Ordering::SeqCst)
    }

    /// Call under the execution's session lock before reaping the leader.
    pub(super) fn mark_leader_exited(&self) {
        self.leader_exited.store(true, Ordering::SeqCst);
    }

    /// Ask the leader to stop. `Ok(false)` when cooperative stop is not
    /// supported here or the leader has already exited.
    ///
    /// # Errors
    /// Signal delivery failures.
    pub(super) fn terminate_leader(&self) -> std::io::Result<bool> {
        if self.leader_exited() {
            return Ok(false);
        }
        self.backend.terminate_leader(self.leader)
    }

    /// Kill everything in the session, the leader included, and verify that
    /// nothing remains.
    ///
    /// # Errors
    /// Discovery or signal failures, or members that survive verification.
    pub(super) fn force_stop(&self) -> std::io::Result<()> {
        self.backend.sweep()
    }

    /// Whether anything is still running in the session.
    ///
    /// # Errors
    /// Discovery failures.
    pub(super) fn occupied(&self) -> std::io::Result<bool> {
        self.backend.occupied()
    }

    /// Leave whatever is still running in the session alone from now on.
    pub(super) fn release_survivors(&self) {
        self.backend.release_survivors();
    }

    /// Give up authority: forget the crash record and release native handles.
    /// Idempotent. Never kills anything by itself on Unix; on Windows closing
    /// the job kills what is left unless [`Self::release_survivors`] ran.
    ///
    /// # Errors
    /// A crash record that cannot be deleted.
    pub(super) fn end_lifetime(&self) -> anyhow::Result<()> {
        self.backend.release();
        let record = self
            .record
            .lock()
            .map_err(|_| anyhow::anyhow!("session record lock poisoned"))?
            .take();
        if let Some(record) = record {
            match std::fs::remove_file(&record) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "cannot delete pane session record {}: {error}",
                        record.display()
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_sweeping() {
        assert_eq!(
            ExecutionSettings::parse(None).unwrap(),
            ExecutionSettings::default()
        );
        assert!(ExecutionSettings::default().sweep_on_exit);
    }

    #[test]
    fn settings_parse_sweep_on_exit() {
        let settings: toml::Value = toml::from_str("[execution]\nsweep_on_exit = false\n").unwrap();
        assert!(
            !ExecutionSettings::parse(Some(&settings))
                .unwrap()
                .sweep_on_exit
        );
    }

    #[test]
    fn settings_reject_unknown_keys_and_wrong_types() {
        let unknown: toml::Value = toml::from_str("[execution]\nnope = true\n").unwrap();
        assert!(ExecutionSettings::parse(Some(&unknown)).is_err());
        let wrong: toml::Value = toml::from_str("[execution]\nsweep_on_exit = 1\n").unwrap();
        assert!(ExecutionSettings::parse(Some(&wrong)).is_err());
        let not_table: toml::Value = toml::from_str("execution = true\n").unwrap();
        assert!(ExecutionSettings::parse(Some(&not_table)).is_err());
    }

    #[test]
    fn execution_and_padding_settings_parse_together() {
        let settings: toml::Value =
            toml::from_str("[execution]\nsweep_on_exit = false\n[padding]\nleft = 2\n").unwrap();
        assert!(
            !ExecutionSettings::parse(Some(&settings))
                .unwrap()
                .sweep_on_exit
        );
        crate::padding::PanePaddingConfig::parse(Some(&settings)).unwrap();
    }

    #[test]
    fn settings_ignore_other_tables() {
        let settings: toml::Value = toml::from_str("[padding]\nenabled = true\n").unwrap();
        assert_eq!(
            ExecutionSettings::parse(Some(&settings)).unwrap(),
            ExecutionSettings::default()
        );
    }

    #[test]
    fn legacy_supervisor_state_is_removed() {
        let runtime = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(runtime.path().join("supervisor-abc.json"), b"{}").unwrap();
        std::fs::write(runtime.path().join("other.json"), b"{}").unwrap();
        std::fs::create_dir_all(state.path().join("auxiliary-images/x/y")).unwrap();
        remove_legacy_supervisor_state(runtime.path(), state.path());
        assert!(!runtime.path().join("supervisor-abc.json").exists());
        assert!(runtime.path().join("other.json").exists());
        assert!(!state.path().join("auxiliary-images").exists());
    }
}
