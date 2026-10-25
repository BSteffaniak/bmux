//! Presenter-owned foreground service. Construction is inert; only admitted Run boots it.
use bmux_config::{BmuxConfig, ConfigPaths};
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
pub(crate) enum ExecutionConnection {
    Host(ConfigPaths),
    Local(Arc<OwnedRuntime>),
}
impl ExecutionConnection {
    pub async fn paths(&self) -> Result<ConfigPaths, String> {
        match self {
            Self::Host(paths) => Ok(paths.clone()),
            Self::Local(runtime) => runtime.ensure_started().await,
        }
    }
    pub fn worker_guard(&self) -> Option<WorkerGuard> {
        match self {
            Self::Host(_) => None,
            Self::Local(runtime) => {
                runtime.workers.fetch_add(1, Ordering::SeqCst);
                Some(WorkerGuard(Arc::clone(runtime)))
            }
        }
    }
}
pub(crate) struct WorkerGuard(Arc<OwnedRuntime>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let last = self.0.workers.fetch_sub(1, Ordering::SeqCst) == 1;
        self.0.settled.notify_one();
        if last && self.0.shutdown_requested.load(Ordering::SeqCst) {
            self.0.schedule_shutdown();
        }
    }
}
pub(crate) struct OwnedRuntime {
    executable: PathBuf,
    root: PathBuf,
    search_roots: Vec<PathBuf>,
    general: bmux_config::GeneralConfig,
    behavior: bmux_config::BehaviorConfig,
    state: Mutex<ServiceState>,
    shutdown_requested: AtomicBool,
    shutdown_scheduled: AtomicBool,
    workers: AtomicUsize,
    settled: Notify,
}
#[derive(Default)]
struct ServiceState {
    child: Option<Child>,
    paths: Option<ConfigPaths>,
    stop_acknowledged: bool,
    closed: bool,
}
impl Drop for ServiceState {
    fn drop(&mut self) {
        // Emergency only: normal success is explicit service stop followed by reap.
        // Never signal an endpoint PID, ancestor, attempt runner, or host daemon.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl OwnedRuntime {
    #[cfg(test)]
    pub fn new(
        executable: PathBuf,
        attempts_root: PathBuf,
        search_roots: Vec<PathBuf>,
    ) -> Arc<Self> {
        Self::with_behavior(
            executable,
            attempts_root,
            search_roots,
            bmux_config::GeneralConfig::default(),
            bmux_config::BehaviorConfig::default(),
        )
    }
    pub fn with_behavior(
        executable: PathBuf,
        attempts_root: PathBuf,
        search_roots: Vec<PathBuf>,
        general: bmux_config::GeneralConfig,
        behavior: bmux_config::BehaviorConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            executable,
            // Durable service state and diagnostics remain discoverable here;
            // socket runtime is separately allocated only on admitted Run.
            root: attempts_root.join(format!(
                "service-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..16]
            )),
            search_roots,
            general,
            behavior,
            state: Mutex::new(ServiceState::default()),
            shutdown_requested: AtomicBool::new(false),
            shutdown_scheduled: AtomicBool::new(false),
            workers: AtomicUsize::new(0),
            settled: Notify::new(),
        })
    }
    fn private_config(&self) -> BmuxConfig {
        let mut config = BmuxConfig {
            general: self.general.clone(),
            behavior: self.behavior.clone(),
            ..BmuxConfig::default()
        };
        config.plugins.enabled = [
            "bmux.playbook",
            "bmux.sessions",
            "bmux.contexts",
            "bmux.clients",
            "bmux.tabs",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        config.plugins.search_paths.clone_from(&self.search_roots);
        config
    }
    fn allocate_paths(&self) -> Result<(ConfigPaths, tempfile::TempDir), String> {
        // Socket paths are ephemeral transport addresses, not durable state.
        // Use the selected temporary root just as managed attempts do; never
        // shorten the user's state path, use a symlink, or fall back to a host.
        let temporary = tempfile::Builder::new()
            .prefix("bps-")
            .tempdir()
            .map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700))
                .map_err(|error| error.to_string())?;
        }
        let paths = ConfigPaths::new(
            self.root.join("config"),
            temporary.path().to_path_buf(),
            self.root.join("data"),
            self.root.join("state"),
        );
        #[cfg(unix)]
        if paths.server_socket().as_os_str().as_encoded_bytes().len() >= 104 {
            return Err(
                "selected temporary root cannot accommodate owned presenter Unix socket paths"
                    .into(),
            );
        }
        Ok((paths, temporary))
    }
    fn cleanup_paths(&self, state: &ServiceState) -> Result<(), String> {
        // Called only once the owned child has exited after stop acknowledgment.
        if let Some(paths) = &state.paths {
            match std::fs::remove_dir_all(&paths.runtime_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        std::fs::remove_dir_all(&self.root).map_err(|error| error.to_string())
    }

    fn command(&self, paths: &ConfigPaths) -> Command {
        let mut command = Command::new(&self.executable);
        // Preserve shell/slot environment. Override only this owned child's paths;
        // do not mutate the parent environment or infer connection from global vars.
        command
            .arg("--config")
            .arg(paths.config_file())
            .arg("--target")
            .arg("local")
            .arg("--runtime")
            .arg("default")
            .args(["server", "start"])
            .env("BMUX_SLOTS_MANIFEST", paths.config_dir.join("slots.toml"))
            .env_remove("BMUX_SLOT_NAME")
            .env_remove("BMUX_TARGET")
            .env_remove("BMUX_CONFIG")
            .env_remove("BMUX_PROFILE")
            .env_remove("BMUX_LOG_DIR")
            .env_remove("BMUX_RECORDINGS_DIR")
            .env("BMUX_NO_BASE_CONFIG", "1")
            .env("BMUX_CONFIG_DIR", &paths.config_dir)
            .env("BMUX_RUNTIME_DIR", &paths.runtime_dir)
            .env("BMUX_RUNTIME_NAME", "default")
            .env("BMUX_DATA_DIR", &paths.data_dir)
            .env("BMUX_STATE_DIR", &paths.state_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
    async fn ensure_started(&self) -> Result<ConfigPaths, String> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err("owned presenter service is closed; no fallback".into());
        }
        if state.child.is_none() {
            if !self.executable.is_absolute()
                || self.search_roots.iter().any(|root| !root.is_absolute())
            {
                return Err(
                    "owned service requires explicit absolute executable and plugin roots".into(),
                );
            }
            std::fs::create_dir_all(&self.root).map_err(|error| error.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
                    .map_err(|error| error.to_string())?;
            }
            let (paths, temporary) = self.allocate_paths()?;
            for directory in [
                &paths.config_dir,
                &paths.runtime_dir,
                &paths.data_dir,
                &paths.state_dir,
            ] {
                std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
            }
            self.private_config()
                .save_to_path(&paths.config_file())
                .map_err(|error| error.to_string())?;
            let diagnostics = std::fs::File::create(self.root.join("server.stderr.log"))
                .map_err(|error| error.to_string())?;
            state.child = Some(
                self.command(&paths)
                    .stderr(Stdio::from(diagnostics))
                    .spawn()
                    .map_err(|error| error.to_string())?,
            );
            // Transfer directory ownership only after the child is retained. An
            // unacknowledged exit must keep its runtime for diagnosis/recovery.
            let _runtime = temporary.keep();
            state.paths = Some(paths);
        }
        let paths = state.paths.clone().ok_or("owned service paths missing")?;
        // Child is stored before awaiting: cancellation never abandons its ownership.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut retry_delay = Duration::from_millis(50);
        let mut last_error = "owned service socket not ready".to_owned();
        loop {
            if let Some(status) = state
                .child
                .as_mut()
                .ok_or("owned service child missing")?
                .try_wait()
                .map_err(|error| error.to_string())?
            {
                state.closed = true;
                return Err(format!(
                    "owned presenter service exited: {status}; diagnostics: {}; no host fallback",
                    self.root.join("server.stderr.log").display()
                ));
            }
            if paths.server_pid_file().exists()
                && let Ok(Ok(mut client)) = tokio::time::timeout(
                    Duration::from_millis(250),
                    bmux_client::BmuxClient::connect_with_paths(
                        &paths,
                        "presenter owned service readiness",
                    ),
                )
                .await
            {
                // Typed service availability, not just a listening socket.
                let readiness = tokio::time::timeout(
                    Duration::from_millis(250),
                    bmux_playbook_plugin_api::playbook_state::client::lookup_preparation(
                        &mut client,
                        "presenter-readiness".to_owned(),
                    ),
                )
                .await;
                if matches!(&readiness, Ok(Ok(Ok(_))))
                    || matches!(
                        &readiness,
                        Ok(Ok(Err(bmux_playbook_plugin_api::playbook_types::PlaybookError::Rejected { message })))
                            if message.starts_with("preparation identity not found")
                    )
                {
                    return Ok(paths);
                }
                last_error = match readiness {
                    Err(_) => "playbook readiness lookup timed out".into(),
                    Ok(Err(error)) => crate::playbook_controller::observation_error(&error),
                    Ok(Ok(Err(error))) => format!("playbook readiness rejected: {error:?}"),
                    Ok(Ok(Ok(_))) => unreachable!("successful readiness returned above"),
                };
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "owned presenter service startup timed out ({last_error}); child retained; diagnostics: {}; no fallback",
                    self.root.join("server.stderr.log").display()
                ));
            }
            // Readiness probes are observations, not preparation retries. Bound
            // missing/incompatible-provider traffic without abandoning the child.
            tokio::time::sleep(
                retry_delay.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
            retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(1));
        }
    }
    pub fn request_shutdown(self: &Arc<Self>) {
        self.shutdown_requested.store(true, Ordering::SeqCst);
        if self.workers.load(Ordering::SeqCst) == 0 {
            self.schedule_shutdown();
        }
    }
    fn schedule_shutdown(self: &Arc<Self>) {
        if self.shutdown_scheduled.swap(true, Ordering::SeqCst) {
            return;
        }
        let runtime = Arc::clone(self);
        // Service ownership survives a UI timeout. Only settled worker guards
        // trigger this continuation; it never aborts the enclosing owner task.
        tokio::spawn(async move {
            if let Err(error) = runtime.shutdown().await {
                eprintln!("Presenter owned service shutdown NOT acknowledged: {error}");
            }
        });
    }
    pub async fn shutdown(&self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(20), async {
            while self.workers.load(Ordering::SeqCst) != 0 {
                self.settled.notified().await;
            }
        })
        .await
        .map_err(|_| "owned service retained: attempts/observers not settled")?;
        let mut state = self.state.lock().await;
        if state.child.is_none() {
            if state.closed && state.paths.is_some() && !state.stop_acknowledged {
                return Err(format!(
                    "owned service shutdown not acknowledged; diagnostics retained at {}",
                    self.root.display()
                ));
            }
            state.closed = true;
            return Ok(());
        }
        if let Some(status) = state
            .child
            .as_mut()
            .ok_or("owned service child missing")?
            .try_wait()
            .map_err(|error| error.to_string())?
        {
            state.child.take();
            if state.stop_acknowledged {
                self.cleanup_paths(&state)?;
                return Ok(());
            }
            state.closed = true;
            return Err(format!(
                "owned service exited before shutdown acknowledgment: {status}; diagnostics retained at {}",
                self.root.display()
            ));
        }
        if !state.closed {
            let paths = state.paths.as_ref().ok_or("owned service paths missing")?;
            let result = tokio::time::timeout(Duration::from_secs(2), async {
                let mut client = bmux_client::BmuxClient::connect_with_paths(
                    paths,
                    "presenter owned service shutdown",
                )
                .await?;
                client.stop_server().await
            })
            .await
            .map_err(|_| "owned service stop timed out; shutdown NOT acknowledged")?
            .map_err(|error| error.to_string());
            result?;
            state.stop_acknowledged = true;
            state.closed = true;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if state
                .child
                .as_mut()
                .ok_or("owned service child missing")?
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
            {
                state.child.take();
                self.cleanup_paths(&state).map_err(|error| {
                    format!("owned service reaped; path cleanup failed: {error}")
                })?;
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("owned service exit not confirmed; child retained".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn construction_is_lazy_and_config_is_explicit() {
        let root = std::env::temp_dir().join(format!("presenter-inert-{}", uuid::Uuid::new_v4()));
        let runtime = OwnedRuntime::new(
            PathBuf::from("/explicit/bmux"),
            root.clone(),
            vec![PathBuf::from("/selected/plugins")],
        );
        assert!(!root.exists());
        assert!(runtime.state.try_lock().unwrap().child.is_none());
        let config = runtime.private_config();
        assert!(
            config
                .plugins
                .enabled
                .iter()
                .any(|id| id == "bmux.playbook")
        );
        assert_eq!(
            config.plugins.search_paths,
            vec![PathBuf::from("/selected/plugins")]
        );
        let paths = ConfigPaths::new(
            root.join("config"),
            root.join("runtime"),
            root.join("data"),
            root.join("state"),
        );
        let command = runtime.command(&paths);
        assert_eq!(command.get_program(), "/explicit/bmux");
        assert!(!command.get_args().any(|arg| arg == "--daemon"));
        let environment: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            environment.get(std::ffi::OsStr::new("BMUX_SLOT_NAME")),
            Some(&None)
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("BMUX_SLOTS_MANIFEST")),
            Some(&Some(paths.config_dir.join("slots.toml").as_os_str()))
        );
        assert!(!root.exists());
    }
    #[tokio::test]
    async fn inert_shutdown_never_creates_service_root() {
        let root =
            std::env::temp_dir().join(format!("presenter-never-started-{}", uuid::Uuid::new_v4()));
        let runtime = OwnedRuntime::new(
            PathBuf::from("/not-resolved-by-path/bmux"),
            root.clone(),
            vec![],
        );
        let connection = ExecutionConnection::Local(Arc::clone(&runtime));
        let guard = connection.worker_guard();
        drop(guard);
        runtime.shutdown().await.unwrap();
        runtime.shutdown().await.unwrap();
        assert!(!root.exists());
        assert!(runtime.ensure_started().await.is_err());
        assert!(!root.exists());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn long_macos_state_path_uses_private_short_socket_and_acknowledged_cleanup() {
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        let state_root = tempfile::tempdir().unwrap();
        let attempts = state_root
            .path()
            .join("Users/braden/Library/Application Support/bmux/State/presenter-playbooks");
        let runtime = OwnedRuntime::new(PathBuf::from("/explicit/bmux"), attempts, vec![]);
        assert!(
            runtime
                .root
                .join("runtime/server.sock")
                .as_os_str()
                .as_encoded_bytes()
                .len()
                >= 104
        );
        assert!(!runtime.root.exists());
        let (paths, temporary) = runtime.allocate_paths().unwrap();
        let socket = paths.server_socket();
        assert!(socket.as_os_str().as_encoded_bytes().len() < 104);
        assert!(!paths.runtime_dir.starts_with(&runtime.root));
        assert!(paths.state_dir.starts_with(&runtime.root));
        assert_eq!(
            std::fs::metadata(&paths.runtime_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
        drop(listener);
        let (other, other_temporary) = runtime.allocate_paths().unwrap();
        assert_ne!(paths.runtime_dir, other.runtime_dir);
        drop(other_temporary);
        assert!(!other.runtime_dir.exists());
        std::fs::create_dir_all(&runtime.root).unwrap();
        // Model an owned, exited child after the stop acknowledgment. Shutdown
        // must remove both its short transport directory and durable service root.
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        child.wait().unwrap();
        let _retained = temporary.keep();
        {
            let mut state = runtime.state.lock().await;
            state.child = Some(child);
            state.paths = Some(paths.clone());
            state.stop_acknowledged = true;
        }
        runtime.shutdown().await.unwrap();
        assert!(!paths.runtime_dir.exists());
        assert!(!runtime.root.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unacknowledged_exit_preserves_short_runtime_and_diagnostics() {
        let root = tempfile::tempdir().unwrap();
        let runtime =
            OwnedRuntime::new(PathBuf::from("/explicit/bmux"), root.path().into(), vec![]);
        let (paths, temporary) = runtime.allocate_paths().unwrap();
        std::fs::create_dir_all(&runtime.root).unwrap();
        std::fs::write(runtime.root.join("server.stderr.log"), "diagnostic").unwrap();
        let mut child = Command::new("sh").args(["-c", "exit 1"]).spawn().unwrap();
        child.wait().unwrap();
        {
            let mut state = runtime.state.lock().await;
            state.child = Some(child);
            state.paths = Some(paths.clone());
        }
        assert!(runtime.shutdown().await.is_err());
        assert!(paths.runtime_dir.exists());
        assert!(runtime.root.join("server.stderr.log").exists());
        drop(temporary);
    }

    #[test]
    fn worker_ownership_is_shared_and_released_explicitly() {
        let runtime = OwnedRuntime::new(
            PathBuf::from("/explicit/bmux"),
            PathBuf::from("/unused"),
            vec![],
        );
        let connection = ExecutionConnection::Local(Arc::clone(&runtime));
        let first = connection.worker_guard();
        let second = connection.clone().worker_guard();
        assert_eq!(runtime.workers.load(Ordering::SeqCst), 2);
        drop(first);
        assert_eq!(runtime.workers.load(Ordering::SeqCst), 1);
        drop(second);
        assert_eq!(runtime.workers.load(Ordering::SeqCst), 0);
        assert!(runtime.state.try_lock().unwrap().child.is_none());
    }
}
