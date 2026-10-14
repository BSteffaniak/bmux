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
            // Short unique basename keeps ordinary selected state roots below
            // sockaddr_un limits without changing the harness discovery layout.
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
            let paths = ConfigPaths::new(
                self.root.join("config"),
                self.root.join("runtime"),
                self.root.join("data"),
                self.root.join("state"),
            );
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                // Portable sockaddr_un limit (BSD 104, Linux 108), including NUL.
                if paths.server_socket().as_os_str().as_bytes().len() >= 104 {
                    return Err(format!(
                        "owned presenter socket path too long: {}; select a shorter state root",
                        paths.server_socket().display()
                    ));
                }
            }
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
            state.paths = Some(paths);
        }
        let paths = state.paths.clone().ok_or("owned service paths missing")?;
        // Child is stored before awaiting: cancellation never abandons its ownership.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
                        readiness,
                        Ok(Ok(Err(bmux_playbook_plugin_api::playbook_types::PlaybookError::Rejected { message })))
                            if message.starts_with("preparation identity not found")
                    )
                {
                    return Ok(paths);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "owned presenter service startup timed out; child retained; diagnostics: {}; no fallback",
                    self.root.join("server.stderr.log").display()
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
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
                std::fs::remove_dir_all(&self.root).map_err(|error| error.to_string())?;
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
                std::fs::remove_dir_all(&self.root).map_err(|error| {
                    format!("owned service reaped; root cleanup failed: {error}")
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
