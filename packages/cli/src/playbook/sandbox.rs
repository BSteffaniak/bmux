//! Sandbox server lifecycle for playbook execution.
//!
//! Provides an ephemeral, isolated bmux server instance that lives only for
//! the duration of a playbook run. This is extracted from (and mirrors) the
//! recording-verify sandbox pattern in `runtime/mod.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use tracing::warn;

use super::types::PluginConfig;

/// Handle to a running sandbox server.
///
/// Implements `Drop` as a defense-in-depth cleanup mechanism: if the struct is
/// dropped without calling `shutdown()` (e.g. due to panic, early return, or
/// signal), it will kill the server process and remove temp dirs.
#[derive(Debug)]
pub struct SandboxServer {
    handle: ServerHandle,
    root_dir: PathBuf,
    cleaned_up: bool,
}

#[derive(Debug)]
enum ServerHandle {
    Foreground {
        child: std::process::Child,
        paths: ConfigPaths,
        _stdout_log: PathBuf,
        #[allow(dead_code)]
        stderr_log: PathBuf,
    },
    Daemon {
        paths: ConfigPaths,
        _stdout_log: PathBuf,
        #[allow(dead_code)]
        stderr_log: PathBuf,
    },
}

impl SandboxServer {
    /// Create and start a new ephemeral sandbox server.
    ///
    /// - `binary`: path to the bmux binary. Falls back to `std::env::current_exe()`
    ///   when `None`.
    /// - `bundled_plugin_ids`: pre-computed list of bundled plugin IDs for the
    ///   disable list. Pass an empty vec if plugin discovery is unavailable.
    pub async fn start(
        shell: Option<&str>,
        plugin_config: &PluginConfig,
        startup_timeout: Duration,
        env: &std::collections::BTreeMap<String, String>,
        env_mode: super::types::SandboxEnvMode,
        binary: Option<&Path>,
        bundled_plugin_ids: &[String],
    ) -> Result<Self> {
        let (paths, root_dir) = create_temp_paths();
        write_sandbox_config(&paths, shell, plugin_config, bundled_plugin_ids)
            .context("failed writing sandbox config")?;

        let bmux_binary = match binary {
            Some(p) => p.to_path_buf(),
            None => std::env::current_exe().context("failed resolving bmux binary path")?,
        };

        let handle = start_sandbox_server(
            &bmux_binary,
            &paths,
            &root_dir,
            startup_timeout,
            env,
            env_mode,
        )
        .await
        .context("failed starting sandbox server")?;

        Ok(Self {
            handle,
            root_dir,
            cleaned_up: false,
        })
    }

    /// Connect a new `BmuxClient` to this sandbox server.
    pub async fn connect(&self, label: &str) -> Result<BmuxClient> {
        BmuxClient::connect_with_paths(self.paths(), label)
            .await
            .map_err(|e| anyhow::anyhow!("failed connecting to sandbox server: {e}"))
    }

    /// Return the `ConfigPaths` for this sandbox.
    pub fn paths(&self) -> &ConfigPaths {
        match &self.handle {
            ServerHandle::Foreground { paths, .. } | ServerHandle::Daemon { paths, .. } => paths,
        }
    }

    /// Return the root temp directory for this sandbox.
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Gracefully shut down the sandbox server and optionally clean up temp dirs.
    pub async fn shutdown(mut self, retain_on_failure: bool) -> Result<()> {
        self.cleaned_up = true;
        let result = self.stop_server().await;
        if !retain_on_failure || result.is_ok() {
            let _ = std::fs::remove_dir_all(&self.root_dir);
        }
        result
    }

    async fn stop_server(&mut self) -> Result<()> {
        // Try graceful IPC stop first.
        if let Ok(mut client) =
            BmuxClient::connect_with_paths(self.paths(), "bmux-playbook-sandbox-stop").await
        {
            let _ = client.stop_server().await;
        }

        match &mut self.handle {
            ServerHandle::Foreground { child, .. } => {
                if wait_for_child_exit(child, Duration::from_secs(3)).await? {
                    return Ok(());
                }
                // Force kill
                let _ = child.kill();
                let _ = wait_for_child_exit(child, Duration::from_secs(1)).await;
                Ok(())
            }
            ServerHandle::Daemon { paths, .. } => {
                if wait_until_server_stopped(paths, Duration::from_secs(3)).await? {
                    return Ok(());
                }
                // Try to kill via PID file
                if let Some(pid) = read_pid_file(paths)? {
                    let _ = try_kill_pid(pid);
                }
                Ok(())
            }
        }
    }
}

impl Drop for SandboxServer {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        // Best-effort synchronous cleanup for abnormal termination (panic, signal, early return).
        warn!("SandboxServer dropped without shutdown — performing emergency cleanup");
        match &mut self.handle {
            ServerHandle::Foreground { child, .. } => {
                let _ = child.kill();
                let _ = child.wait();
            }
            ServerHandle::Daemon { paths, .. } => {
                if let Ok(Some(pid)) = read_pid_file(paths) {
                    let _ = try_kill_pid(pid);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root_dir);
    }
}

// ── Temp directory creation ──────────────────────────────────────────────────

fn create_temp_paths() -> (ConfigPaths, PathBuf) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let root = std::env::temp_dir().join(format!("bpb-{nanos:x}"));
    let paths = ConfigPaths::new(
        root.join("c"),
        root.join("r"),
        root.join("d"),
        root.join("s"),
    );
    (paths, root)
}

// ── Sandbox environment ─────────────────────────────────────────────────────

/// Apply the sandbox environment to a server command.
///
/// - **Inherit mode** (default): keeps the parent environment, then overlays
///   the `BMUX_*` path overrides and deterministic defaults for `TERM`, `LANG`,
///   `LC_ALL`, and `HOME`. Any explicit `@env` entries are applied last.
///
/// - **Clean mode**: calls `env_clear()` first, then sets only `BMUX_*` paths,
///   deterministic defaults, `PATH`/`USER`/`SHELL` from the parent, and
///   explicit `@env` entries. Maximally deterministic.
fn apply_sandbox_env(
    cmd: &mut ProcessCommand,
    paths: &ConfigPaths,
    root_dir: &Path,
    env: &std::collections::BTreeMap<String, String>,
    env_mode: super::types::SandboxEnvMode,
) {
    if env_mode == super::types::SandboxEnvMode::Clean {
        cmd.env_clear();
    }

    // BMUX isolation paths (always set).
    cmd.env("BMUX_CONFIG_DIR", &paths.config_dir);
    cmd.env("BMUX_RUNTIME_DIR", &paths.runtime_dir);
    cmd.env("BMUX_DATA_DIR", &paths.data_dir);
    cmd.env("BMUX_STATE_DIR", &paths.state_dir);

    // Deterministic defaults — override the parent value (Inherit) or provide
    // the only value (Clean) for variables that affect terminal/locale behavior.
    //
    // HOME defaults to the sandbox temp dir so shells don't read the user's
    // dotfiles (which could produce non-deterministic prompts/output).
    if !env.contains_key("HOME") {
        cmd.env("HOME", root_dir);
    }
    if !env.contains_key("TERM") {
        cmd.env("TERM", "xterm-256color");
    }
    if !env.contains_key("LANG") {
        cmd.env("LANG", "C.UTF-8");
    }
    if !env.contains_key("LC_ALL") {
        cmd.env("LC_ALL", "C.UTF-8");
    }

    // In Clean mode, PATH/USER/SHELL must be explicitly inherited because
    // env_clear() removed them. In Inherit mode they're already present.
    if env_mode == super::types::SandboxEnvMode::Clean {
        if !env.contains_key("PATH") {
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", path);
            }
        }
        if !env.contains_key("USER") {
            if let Ok(user) = std::env::var("USER") {
                cmd.env("USER", user);
            }
        }
        if !env.contains_key("SHELL") {
            if let Ok(shell) = std::env::var("SHELL") {
                cmd.env("SHELL", shell);
            }
        }
    }

    // User-specified overrides (from @env directives or [playbook.env]).
    for (key, value) in env {
        cmd.env(key, value);
    }
}

// ── Config writing ───────────────────────────────────────────────────────────

fn write_sandbox_config(
    paths: &ConfigPaths,
    shell: Option<&str>,
    plugin_config: &PluginConfig,
    bundled_plugin_ids: &[String],
) -> Result<()> {
    let config_path = paths.config_file();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating config dir {}", parent.display()))?;
    }

    let mut toml = String::new();

    // Shell override
    if let Some(shell) = shell {
        toml.push_str(&format!("[general]\ndefault_shell = '{shell}'\n\n"));
    }

    // Plugin configuration — build disabled list
    let disabled = build_plugin_disabled_list(plugin_config, bundled_plugin_ids);
    let enabled = build_plugin_enabled_list(plugin_config);

    if !disabled.is_empty() || !enabled.is_empty() {
        toml.push_str("[plugins]\n");
        if !disabled.is_empty() {
            let quoted: Vec<String> = disabled.iter().map(|id| format!("'{id}'")).collect();
            toml.push_str(&format!("disabled = [{}]\n", quoted.join(", ")));
        }
        if !enabled.is_empty() {
            let quoted: Vec<String> = enabled.iter().map(|id| format!("'{id}'")).collect();
            toml.push_str(&format!("enabled = [{}]\n", quoted.join(", ")));
        }
    }

    std::fs::write(&config_path, toml)
        .with_context(|| format!("failed writing sandbox config {}", config_path.display()))
}

fn build_plugin_disabled_list(plugin_config: &PluginConfig, bundled_ids: &[String]) -> Vec<String> {
    // If the user explicitly enabled specific plugins, disable everything else
    // by using a blanket disable approach: disable all discovered bundled plugin IDs
    // except those in the enable list.
    if !plugin_config.enable.is_empty() {
        let mut disabled: Vec<String> = bundled_ids
            .iter()
            .filter(|id| !plugin_config.enable.contains(id))
            .cloned()
            .collect();
        // Also include explicitly disabled plugins
        for id in &plugin_config.disable {
            if !disabled.contains(id) {
                disabled.push(id.clone());
            }
        }
        disabled.sort();
        disabled
    } else if plugin_config.disable.is_empty() {
        // Default: disable all bundled plugins for clean deterministic baseline
        let mut all = bundled_ids.to_vec();
        all.sort();
        all
    } else {
        let mut list = plugin_config.disable.clone();
        list.sort();
        list
    }
}

fn build_plugin_enabled_list(plugin_config: &PluginConfig) -> Vec<String> {
    if plugin_config.enable.is_empty() {
        Vec::new()
    } else {
        let mut list = plugin_config.enable.clone();
        list.sort();
        list
    }
}

// ── Server startup ───────────────────────────────────────────────────────────

async fn start_sandbox_server(
    binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
    env: &std::collections::BTreeMap<String, String>,
    env_mode: super::types::SandboxEnvMode,
) -> Result<ServerHandle> {
    match start_foreground(binary, paths, root_dir, timeout, env, env_mode).await {
        Ok(handle) => Ok(handle),
        Err(fg_error) => {
            warn!("playbook sandbox foreground startup failed, falling back to daemon: {fg_error}");
            start_daemon(binary, paths, root_dir, timeout, env, env_mode)
                .await
                .with_context(|| {
                    format!(
                        "sandbox startup failed in foreground and daemon; foreground error: {fg_error:#}"
                    )
                })
        }
    }
}

async fn start_foreground(
    binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
    env: &std::collections::BTreeMap<String, String>,
    env_mode: super::types::SandboxEnvMode,
) -> Result<ServerHandle> {
    let logs_dir = root_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let stdout_log = logs_dir.join("sandbox-server.stdout.log");
    let stderr_log = logs_dir.join("sandbox-server.stderr.log");

    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stdout_log)?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_log)?;

    let mut cmd = ProcessCommand::new(binary);
    cmd.arg("server").arg("start");

    apply_sandbox_env(&mut cmd, paths, root_dir, env, env_mode);

    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .with_context(|| format!("failed spawning sandbox server {}", binary.display()))?;

    let mut handle = ServerHandle::Foreground {
        child,
        paths: paths.clone(),
        _stdout_log: stdout_log,
        stderr_log: stderr_log.clone(),
    };

    match wait_for_server_ready(paths, timeout, handle.child_mut()).await {
        Ok(()) => Ok(handle),
        Err(error) => {
            let excerpt = read_log_excerpt(&stderr_log);
            if let ServerHandle::Foreground { ref mut child, .. } = handle {
                let _ = child.kill();
            }
            Err(error).with_context(|| format!("sandbox startup failed (stderr: {excerpt})"))
        }
    }
}

async fn start_daemon(
    binary: &Path,
    paths: &ConfigPaths,
    root_dir: &Path,
    timeout: Duration,
    env: &std::collections::BTreeMap<String, String>,
    env_mode: super::types::SandboxEnvMode,
) -> Result<ServerHandle> {
    let logs_dir = root_dir.join("logs");
    std::fs::create_dir_all(&logs_dir)?;
    let stdout_log = logs_dir.join("sandbox-server-daemon.stdout.log");
    let stderr_log = logs_dir.join("sandbox-server-daemon.stderr.log");

    let mut cmd = ProcessCommand::new(binary);
    cmd.arg("server").arg("start").arg("--daemon");
    apply_sandbox_env(&mut cmd, paths, root_dir, env, env_mode);

    let output = cmd.output().context("failed starting sandbox daemon")?;

    std::fs::write(&stdout_log, &output.stdout)?;
    std::fs::write(&stderr_log, &output.stderr)?;

    if !output.status.success() {
        let excerpt = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("sandbox daemon start failed: {excerpt}");
    }

    wait_for_server_ready(paths, timeout, None).await?;

    Ok(ServerHandle::Daemon {
        paths: paths.clone(),
        _stdout_log: stdout_log,
        stderr_log,
    })
}

// ── Server readiness and lifecycle helpers ────────────────────────────────────

async fn wait_for_server_ready(
    paths: &ConfigPaths,
    timeout: Duration,
    mut child: Option<&mut std::process::Child>,
) -> Result<()> {
    let start = Instant::now();
    let mut poll_delay = Duration::from_millis(50);

    loop {
        match BmuxClient::connect_with_paths(paths, "bmux-playbook-sandbox-ready").await {
            Ok(_) => return Ok(()),
            Err(_) if start.elapsed() < timeout => {
                if let Some(ref mut child) = child {
                    if let Some(status) = child.try_wait()? {
                        anyhow::bail!("sandbox server exited before ready (status: {status})");
                    }
                }
                tokio::time::sleep(poll_delay).await;
                poll_delay = (poll_delay * 2).min(Duration::from_millis(250));
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

async fn wait_until_server_stopped(paths: &ConfigPaths, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match BmuxClient::connect_with_paths(paths, "bmux-playbook-sandbox-stop-check").await {
            Ok(_) => tokio::time::sleep(Duration::from_millis(80)).await,
            Err(_) => return Ok(true),
        }
    }
    Ok(false)
}

async fn wait_for_child_exit(child: &mut std::process::Child, timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    Ok(child.try_wait()?.is_some())
}

fn read_pid_file(paths: &ConfigPaths) -> Result<Option<u32>> {
    let pid_file = paths.server_pid_file();
    match std::fs::read_to_string(&pid_file) {
        Ok(content) => Ok(content.trim().parse::<u32>().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed reading {}", pid_file.display())),
    }
}

fn try_kill_pid(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        let status = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .context("failed to execute kill command")?;
        Ok(status.success())
    }

    #[cfg(windows)]
    {
        let status = std::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/T")
            .arg("/F")
            .status()
            .context("failed to execute taskkill command")?;
        Ok(status.success())
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(false)
    }
}

fn read_log_excerpt(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().last().map(str::to_string))
        .filter(|line| !line.trim().is_empty())
        .unwrap_or_else(|| "<empty>".to_string())
}

impl ServerHandle {
    fn child_mut(&mut self) -> Option<&mut std::process::Child> {
        match self {
            Self::Foreground { child, .. } => Some(child),
            Self::Daemon { .. } => None,
        }
    }
}
