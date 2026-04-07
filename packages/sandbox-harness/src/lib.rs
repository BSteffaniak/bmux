#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use anyhow::{Context, Result};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use bmux_server::BmuxServer;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;
use tracing::warn;

/// Default timeout while waiting for the sandbox server to become ready.
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const CLEANUP_MIN_AGE: Duration = Duration::from_secs(300);
const SANDBOX_PREFIX: &str = "bsh-";

/// Configuration options for sandbox startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxHarnessOptions {
    /// Maximum time to wait for the sandbox server to accept client connections.
    pub startup_timeout: Duration,
}

impl Default for SandboxHarnessOptions {
    fn default() -> Self {
        Self {
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
        }
    }
}

/// Orphan cleanup result entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupEntry {
    pub path: String,
    pub age_secs: u64,
    pub removed: bool,
}

/// Isolated in-process bmux server harness.
pub struct SandboxHarness {
    root_dir: PathBuf,
    paths: ConfigPaths,
    server: BmuxServer,
    server_task: Option<JoinHandle<Result<()>>>,
    cleaned_up: bool,
}

impl SandboxHarness {
    /// Start a new sandbox harness with default options.
    ///
    /// # Errors
    /// Returns an error if startup fails or the server is not ready in time.
    pub async fn start() -> Result<Self> {
        Self::start_with_options(SandboxHarnessOptions::default()).await
    }

    /// Start a new sandbox harness with explicit options.
    ///
    /// # Errors
    /// Returns an error if startup fails or the server is not ready in time.
    pub async fn start_with_options(options: SandboxHarnessOptions) -> Result<Self> {
        let (paths, root_dir) = create_temp_paths();
        ensure_sandbox_dirs(&paths)?;
        write_pid_marker(&root_dir)?;

        let server = BmuxServer::from_config_paths(&paths);
        let mut server_task = tokio::spawn({
            let server = server.clone();
            async move { server.run().await }
        });

        if let Err(error) = wait_for_server_ready(&paths, options.startup_timeout).await {
            server.request_shutdown();
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut server_task).await;
            let _ = std::fs::remove_dir_all(&root_dir);
            return Err(error).context("sandbox server failed to become ready");
        }

        Ok(Self {
            root_dir,
            paths,
            server,
            server_task: Some(server_task),
            cleaned_up: false,
        })
    }

    /// Connect a new client to this sandbox server.
    ///
    /// # Errors
    /// Returns an error if the connection fails.
    pub async fn connect(&self, label: &str) -> Result<BmuxClient> {
        BmuxClient::connect_with_paths(&self.paths, label)
            .await
            .map_err(|error| anyhow::anyhow!("failed connecting to sandbox server: {error}"))
    }

    /// Return the sandbox configuration paths.
    #[must_use]
    pub const fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    /// Return the sandbox root directory path.
    #[must_use]
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Return BMUX environment overrides for this sandbox.
    #[must_use]
    pub fn env_overrides(&self) -> BTreeMap<&'static str, String> {
        let mut values = BTreeMap::new();
        values.insert(
            "BMUX_CONFIG_DIR",
            self.paths.config_dir.to_string_lossy().to_string(),
        );
        values.insert(
            "BMUX_RUNTIME_DIR",
            self.paths.runtime_dir.to_string_lossy().to_string(),
        );
        values.insert(
            "BMUX_DATA_DIR",
            self.paths.data_dir.to_string_lossy().to_string(),
        );
        values.insert(
            "BMUX_STATE_DIR",
            self.paths.state_dir.to_string_lossy().to_string(),
        );
        values
    }

    /// Apply BMUX environment overrides to a process command.
    pub fn apply_env(&self, command: &mut ProcessCommand) {
        command.env("BMUX_CONFIG_DIR", &self.paths.config_dir);
        command.env("BMUX_RUNTIME_DIR", &self.paths.runtime_dir);
        command.env("BMUX_DATA_DIR", &self.paths.data_dir);
        command.env("BMUX_STATE_DIR", &self.paths.state_dir);
    }

    /// Gracefully shut down the sandbox and clean up the temp root directory.
    ///
    /// # Errors
    /// Returns an error if server shutdown fails.
    pub async fn shutdown(mut self, retain_root_on_failure: bool) -> Result<()> {
        self.cleaned_up = true;
        let result = self.stop_server().await;
        if !retain_root_on_failure || result.is_ok() {
            let _ = std::fs::remove_dir_all(&self.root_dir);
        }
        result
    }

    async fn stop_server(&mut self) -> Result<()> {
        if let Ok(mut client) =
            BmuxClient::connect_with_paths(&self.paths, "bmux-sandbox-stop").await
        {
            let _ = client.stop_server().await;
        }

        self.server.request_shutdown();

        let Some(mut task) = self.server_task.take() else {
            return Ok(());
        };

        if let Ok(join_result) = tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task).await {
            match join_result {
                Ok(run_result) => run_result.context("sandbox server exited with error"),
                Err(error) => Err(anyhow::anyhow!("sandbox server task join failed: {error}")),
            }
        } else {
            task.abort();
            let _ = task.await;
            Ok(())
        }
    }
}

impl Drop for SandboxHarness {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }

        warn!("SandboxHarness dropped without shutdown; aborting server task");
        self.server.request_shutdown();
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(&self.root_dir);
    }
}

/// Remove orphaned sandbox harness directories under the system temp root.
///
/// Only directories prefixed with `bsh-` and older than five minutes are
/// considered for removal.
#[must_use]
pub fn cleanup_orphaned_harnesses(dry_run: bool) -> (usize, Vec<CleanupEntry>) {
    let temp_dir = std::env::temp_dir();
    let mut scanned = 0;
    let mut entries = Vec::new();
    let now = SystemTime::now();

    let Ok(dir_entries) = std::fs::read_dir(&temp_dir) else {
        return (0, entries);
    };

    for entry in dir_entries {
        let Ok(entry) = entry else { continue };

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(SANDBOX_PREFIX) || !entry.path().is_dir() {
            continue;
        }

        scanned += 1;

        let age = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default();

        if age < CLEANUP_MIN_AGE {
            continue;
        }

        let root_path = entry.path();
        if harness_process_alive(&root_path) || harness_socket_alive(&root_path) {
            continue;
        }

        let removed = if dry_run {
            false
        } else {
            std::fs::remove_dir_all(&root_path).is_ok()
        };

        entries.push(CleanupEntry {
            path: root_path.to_string_lossy().to_string(),
            age_secs: age.as_secs(),
            removed,
        });
    }

    (scanned, entries)
}

fn create_temp_paths() -> (ConfigPaths, PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let root =
        std::env::temp_dir().join(format!("{SANDBOX_PREFIX}{nanos:x}-{}", std::process::id()));
    let paths = ConfigPaths::new(
        root.join("c"),
        root.join("r"),
        root.join("d"),
        root.join("s"),
    );
    (paths, root)
}

fn ensure_sandbox_dirs(paths: &ConfigPaths) -> Result<()> {
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("failed creating {}", paths.config_dir.display()))?;
    std::fs::create_dir_all(&paths.runtime_dir)
        .with_context(|| format!("failed creating {}", paths.runtime_dir.display()))?;
    std::fs::create_dir_all(&paths.data_dir)
        .with_context(|| format!("failed creating {}", paths.data_dir.display()))?;
    std::fs::create_dir_all(&paths.state_dir)
        .with_context(|| format!("failed creating {}", paths.state_dir.display()))?;
    Ok(())
}

fn write_pid_marker(root_dir: &Path) -> Result<()> {
    let marker_path = root_dir.join("harness.pid");
    std::fs::write(&marker_path, std::process::id().to_string())
        .with_context(|| format!("failed writing {}", marker_path.display()))
}

async fn wait_for_server_ready(paths: &ConfigPaths, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut poll_delay = Duration::from_millis(50);

    loop {
        match BmuxClient::connect_with_paths(paths, "bmux-sandbox-ready").await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                tokio::time::sleep(poll_delay).await;
                poll_delay = poll_delay.saturating_mul(2).min(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "sandbox server not ready within {}s: {error}",
                    timeout.as_secs()
                ));
            }
        }
    }
}

fn harness_process_alive(root_dir: &Path) -> bool {
    let marker_path = root_dir.join("harness.pid");
    std::fs::read_to_string(marker_path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok())
        .is_some_and(is_pid_alive)
}

fn harness_socket_alive(root_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let socket_path = root_dir.join("r").join("server.sock");
        if !socket_path.exists() {
            return false;
        }
        std::os::unix::net::UnixStream::connect(socket_path).is_ok()
    }

    #[cfg(not(unix))]
    {
        let _ = root_dir;
        false
    }
}

fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        ProcessCommand::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    {
        ProcessCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::SandboxHarness;

    #[tokio::test]
    async fn sandbox_harness_starts_and_connects() {
        let sandbox = SandboxHarness::start()
            .await
            .expect("sandbox harness should start");
        let mut client = sandbox
            .connect("bmux-sandbox-harness-test")
            .await
            .expect("sandbox client should connect");
        let _client_id = client.whoami().await.expect("whoami should succeed");
        sandbox
            .shutdown(false)
            .await
            .expect("sandbox shutdown should succeed");
    }
}
