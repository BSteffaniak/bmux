//! Sandbox server lifecycle for playbook execution.
//!
//! Provides an ephemeral, isolated bmux server instance that lives only for
//! the duration of a playbook run. This is extracted from (and mirrors) the
//! recording-verify sandbox pattern in `runtime/mod.rs`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bmux_client::BmuxClient;
use bmux_config::ConfigPaths;
use tracing::warn;

use super::types::PluginConfig;
use crate::host::{HostAdapter, SandboxManifestUpdate};
use std::sync::Arc;

/// Handle to a running sandbox server.
///
/// Dropping without verified shutdown preserves runtime/recovery artifacts.
/// Only the directly owned foreground child is signalled as a fallback.
#[derive(Debug)]
pub struct SandboxServer {
    host: Arc<dyn HostAdapter>,
    handle: ServerHandle,
    root_dir: PathBuf,
    cleaned_up: bool,
    identity: serde_json::Value,
    retirement_fence: Option<std::fs::File>,
}

#[derive(Debug)]
enum ServerHandle {
    Foreground {
        child: std::process::Child,
        paths: ConfigPaths,
        _stdout_log: PathBuf,
        stderr_log: PathBuf,
    },
    Daemon {
        paths: ConfigPaths,
        _stdout_log: PathBuf,
        stderr_log: PathBuf,
    },
}

pub struct SandboxStartOptions<'a> {
    pub shell: Option<&'a str>,
    pub input_profile: Option<super::types::InputProfile>,
    pub plugin_config: &'a PluginConfig,
    /// Require recording only for this explicitly requested sandbox run.
    pub recording_requested: bool,
    pub startup_timeout: Duration,
    pub env: &'a std::collections::BTreeMap<String, String>,
    pub env_mode: super::types::SandboxEnvMode,
    pub binary: Option<&'a Path>,
    pub sandbox_config_file: Option<&'a Path>,
    pub bundled_plugin_ids: &'a [String],
}

impl SandboxServer {
    /// Create and start a new ephemeral sandbox server.
    ///
    /// - `binary`: path to the bmux binary. Falls back to `std::env::current_exe()`
    ///   when `None`.
    /// - `bundled_plugin_ids`: pre-computed list of bundled plugin IDs for the
    ///   disable list. Pass an empty vec if plugin discovery is unavailable.
    /// # Errors
    ///
    /// Returns an error if the sandbox server fails to start.
    pub async fn start(
        options: SandboxStartOptions<'_>,
        host: Arc<dyn HostAdapter>,
    ) -> Result<Self> {
        let (paths, root_dir) = if let Some(root) = std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT") {
            let root = PathBuf::from(root).join("sandbox");
            anyhow::ensure!(root.is_absolute(), "managed sandbox root must be absolute");
            create_private_directory(&root)
                .context("managed sandbox root already exists or cannot be created")?;
            let runtime = std::env::var_os("BMUX_PLAYBOOK_MANAGED_RUNTIME_DIR")
                .map(PathBuf::from)
                .context("managed runtime path missing")?;
            anyhow::ensure!(
                runtime.is_absolute(),
                "managed runtime path must be absolute"
            );
            (
                ConfigPaths::new(root.join("c"), runtime, root.join("d"), root.join("s")),
                root,
            )
        } else {
            let (paths, root) =
                create_temp_paths().context("failed allocating private sandbox root")?;
            create_private_directory(&paths.runtime_dir)?;
            (paths, root)
        };
        write_sandbox_config_with_options(&paths, &options)
            .context("failed writing sandbox config")?;

        let bmux_binary = if std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT").is_some() {
            // Managed execution must use the already-selected isolated worker executable.
            host.executable()?
        } else {
            options
                .binary
                .map_or_else(|| host.executable(), |path| Ok(path.to_path_buf()))?
        };

        write_playbook_manifest(
            host.as_ref(),
            &root_dir,
            &paths,
            &bmux_binary,
            &["server".to_string(), "start".to_string()],
            options.env_mode,
            "running",
            None,
            true,
        )?;
        let _ = host.write_sandbox_lock(&root_dir, std::process::id());

        let handle = start_sandbox_server(
            &bmux_binary,
            &paths,
            &root_dir,
            options.startup_timeout,
            options.env,
            options.env_mode,
        )
        .await
        .context("failed starting sandbox server")?;

        let mut sandbox = Self {
            host,
            handle,
            root_dir,
            cleaned_up: false,
            identity: serde_json::Value::Null,
            retirement_fence: None,
        };
        // Establish the owning Drop guard before reading fallible metadata.
        sandbox.identity = serde_json::from_slice(&std::fs::read(
            sandbox.paths().runtime_dir.join("server-meta.json"),
        )?)?;
        if let ServerHandle::Foreground { child, .. } = &sandbox.handle {
            anyhow::ensure!(
                sandbox.identity["pid"].as_u64() == Some(u64::from(child.id())),
                "sandbox metadata does not match owned foreground child"
            );
        }
        Ok(sandbox)
    }

    /// Connect a new `BmuxClient` to this sandbox server.
    /// # Errors
    ///
    /// Returns an error if the connection to the sandbox server fails.
    pub async fn connect(&self, label: &str) -> Result<BmuxClient> {
        BmuxClient::connect_with_paths(self.paths(), label)
            .await
            .map_err(|e| anyhow::anyhow!("failed connecting to sandbox server: {e}"))
    }

    /// Return the `ConfigPaths` for this sandbox.
    #[must_use]
    pub const fn paths(&self) -> &ConfigPaths {
        match &self.handle {
            ServerHandle::Foreground { paths, .. } | ServerHandle::Daemon { paths, .. } => paths,
        }
    }

    /// Return the root temp directory for this sandbox.
    #[must_use]
    pub fn root_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Return the sandbox server stderr log path.
    #[must_use]
    pub fn stderr_log_path(&self) -> &Path {
        match &self.handle {
            ServerHandle::Foreground { stderr_log, .. }
            | ServerHandle::Daemon { stderr_log, .. } => stderr_log,
        }
    }

    /// Gracefully shut down the sandbox server and optionally clean up temp dirs.
    /// # Errors
    ///
    /// Returns an error if the sandbox server fails to shut down cleanly.
    pub async fn shutdown(mut self, retain_on_failure: bool) -> Result<()> {
        let result = self.stop_server().await;
        if result.is_ok() {
            self.cleaned_up = true;
        }
        if result.is_ok()
            && let Some(root) = std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT")
        {
            std::fs::remove_file(PathBuf::from(root).join("sandbox-child-owned"))
                .context("failed recording sandbox child cleanup")?;
        }
        let status = if result.is_ok() {
            "succeeded"
        } else {
            "failed"
        };
        let keep = retain_sandbox_after_shutdown(retain_on_failure, result.is_ok());
        let _ = write_playbook_manifest(
            self.host.as_ref(),
            &self.root_dir,
            self.paths(),
            &self.host.executable()?,
            &["server".to_string(), "stop".to_string()],
            super::types::SandboxEnvMode::Inherit,
            status,
            None,
            keep,
        );
        let _ = self.host.clear_sandbox_lock(&self.root_dir);
        let full_retention =
            std::env::var_os("BMUX_TEST_KEEP_ARTIFACTS").is_some_and(|value| value == "1");
        let managed = std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT").is_some();
        if result.is_ok() && !managed && !full_retention {
            // The lifetime fence remains held. No executable/recovery artifacts
            // survive a failed playbook whose execution domain was retired.
            std::fs::remove_dir_all(&self.root_dir)?;
            if retain_on_failure {
                create_private_directory(&self.root_dir)?;
                std::fs::write(
                    self.root_dir.join("README.txt"),
                    "Playbook failed; sandbox execution retirement verified. See failure_captures in the playbook report. Runtime images, credentials, databases and raw logs are not retained. Set BMUX_TEST_KEEP_ARTIFACTS=1 for full retention.\n",
                )?;
            }
        } else if !keep && !full_retention {
            std::fs::remove_dir_all(&self.root_dir)?;
        }
        result
    }

    async fn stop_server(&mut self) -> Result<()> {
        use bmux_sessions_plugin_api::{sessions_commands, sessions_state};
        let paths = self.paths().clone();
        let identity: serde_json::Value =
            serde_json::from_slice(&std::fs::read(paths.runtime_dir.join("server-meta.json"))?)?;
        anyhow::ensure!(identity == self.identity, "sandbox server identity changed");
        let mut client =
            BmuxClient::connect_with_paths(&paths, "bmux-playbook-sandbox-stop").await?;
        let cleanup = async {
            for session in sessions_state::client::list_sessions(&mut client).await? {
                let result = sessions_commands::client::kill_session(
                    &mut client,
                    sessions_state::SessionSelector {
                        id: Some(session.id),
                        name: None,
                    },
                    true,
                )
                .await?;
                result.map_err(|error| anyhow::anyhow!("sandbox session retirement: {error:?}"))?;
            }
            client.stop_server().await?;
            Ok::<(), anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(10), cleanup).await??;
        if let ServerHandle::Foreground { child, .. } = &mut self.handle {
            anyhow::ensure!(
                wait_for_child_exit(child, Duration::from_secs(3)).await?,
                "sandbox child exit unconfirmed"
            );
        }
        let fence = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.runtime_dir.join("server-lifetime.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if fence.try_lock().is_ok() {
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "sandbox lifetime fence remains held"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(
            paths.runtime_dir.join("shutdown-result.json"),
        )?)?;
        anyhow::ensure!(
            receipt["pid"] == identity["pid"]
                && receipt["started_at_epoch_ms"] == identity["started_at_epoch_ms"]
                && receipt.get("error") == Some(&serde_json::Value::Null),
            "matching successful sandbox shutdown receipt missing"
        );
        match std::fs::read_dir(paths.state_dir.join("pane-executions-v4")) {
            Ok(mut entries) => {
                anyhow::ensure!(entries.next().is_none(), "sandbox recovery records remain")
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.retirement_fence = Some(fence);
        Ok(())
    }
}

const fn retain_sandbox_after_shutdown(retain_requested: bool, shutdown_succeeded: bool) -> bool {
    retain_requested || !shutdown_succeeded
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
            // No process handle: PID-file identity alone never authorizes a signal.
            ServerHandle::Daemon { .. } => {}
        }
        warn!(root = %self.root_dir.display(), "sandbox retirement unconfirmed; preserving runtime and recovery artifacts");
    }
}

// ── Temp directory creation ──────────────────────────────────────────────────

#[cfg(unix)]
pub(crate) fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    // Exclusive creation: never adopt, follow or repair an existing root.
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
pub(crate) fn create_private_directory(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new().create(path)
}

// Candidate names are not ownership: only successful exclusive creation is.
// Retry collisions without inspecting, repairing or deleting the occupied path.
fn allocate_private_root(mut candidate: impl FnMut() -> PathBuf) -> std::io::Result<PathBuf> {
    for _ in 0..32 {
        let root = candidate();
        match create_private_directory(&root) {
            Ok(()) => return Ok(root),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "private sandbox root collision retry budget exhausted",
    ))
}

fn create_temp_paths() -> std::io::Result<(ConfigPaths, PathBuf)> {
    let parent = std::env::temp_dir();
    let root =
        allocate_private_root(|| parent.join(format!("bpb-{}", uuid::Uuid::new_v4().simple())))?;
    let paths = ConfigPaths::new(
        root.join("c"),
        root.join("r"),
        root.join("d"),
        root.join("s"),
    );
    Ok((paths, root))
}

#[allow(clippy::too_many_arguments)]
fn write_playbook_manifest(
    host: &dyn HostAdapter,
    root_dir: &Path,
    paths: &ConfigPaths,
    bmux_binary: &Path,
    command: &[String],
    env_mode: super::types::SandboxEnvMode,
    status: &str,
    exit_code: Option<i32>,
    kept: bool,
) -> Result<()> {
    host.write_sandbox_manifest(SandboxManifestUpdate {
        root: root_dir,
        paths,
        executable: bmux_binary,
        command,
        env_mode,
        status,
        exit_code,
        kept,
    })?;
    host.register_sandbox(root_dir)
}

/// Apply the sandbox environment to a server command.
///
/// - **Inherit mode** (default): keeps the parent environment, then overlays
///   the `BMUX_*` path overrides and deterministic defaults for `TERM`, `LANG`,
///   `LC_ALL`, and `HOME`. Any explicit `@env` entries are applied last.
///
/// - **Clean mode**: calls `env_clear()` first, then sets only `BMUX_*` paths,
///   deterministic defaults, `PATH`/`USER`/`SHELL` from the parent, and
///   explicit `@env` entries. Maximally deterministic.
pub(crate) fn apply_sandbox_env(
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
    cmd.env("BMUX_LOG_DIR", root_dir.join("logs"));

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
        if !env.contains_key("PATH")
            && let Ok(path) = std::env::var("PATH")
        {
            cmd.env("PATH", path);
        }
        if !env.contains_key("USER")
            && let Ok(user) = std::env::var("USER")
        {
            cmd.env("USER", user);
        }
        if !env.contains_key("SHELL")
            && let Ok(shell) = std::env::var("SHELL")
        {
            cmd.env("SHELL", shell);
        }
    }

    // User-specified overrides (from @env directives or [playbook.env]).
    for (key, value) in env {
        cmd.env(key, value);
    }
    // Explicit deck overrides cannot change the sandbox's runtime authority.
    if std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT").is_some() {
        cmd.env("BMUX_NO_BASE_CONFIG", "1");
        cmd.env_remove("BMUX_SLOT");
        cmd.env_remove("BMUX_RUNTIME");
        cmd.env_remove("BMUX_CONFIG");
        cmd.env_remove("BMUX_PLAYBOOK_ALLOWED_ROOT");
    }
    cmd.env_remove("BMUX_PLAYBOOK_MANAGED_ROOT");
    cmd.env_remove("BMUX_PLAYBOOK_MANAGED_GENERATION");
    cmd.env_remove("BMUX_PLAYBOOK_MANAGED_RUNTIME_DIR");
    cmd.env("BMUX_CONFIG_DIR", &paths.config_dir);
    cmd.env("BMUX_RUNTIME_DIR", &paths.runtime_dir);
    cmd.env("BMUX_DATA_DIR", &paths.data_dir);
    cmd.env("BMUX_STATE_DIR", &paths.state_dir);
    cmd.env("BMUX_LOG_DIR", root_dir.join("logs"));
}

// ── Config writing ───────────────────────────────────────────────────────────

pub(crate) fn write_sandbox_config_with_options(
    paths: &ConfigPaths,
    options: &SandboxStartOptions<'_>,
) -> Result<()> {
    let SandboxStartOptions {
        shell,
        input_profile,
        plugin_config,
        recording_requested,
        sandbox_config_file,
        bundled_plugin_ids,
        ..
    } = *options;
    let config_path = paths.config_file();
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating config dir {}", parent.display()))?;
    }

    let mut toml = read_sandbox_config(sandbox_config_file)?;

    let explicit_plugin_directives =
        !plugin_config.enable.is_empty() || !plugin_config.disable.is_empty();
    let should_write_generated_plugin_config =
        sandbox_config_file.is_none() || explicit_plugin_directives;
    if sandbox_config_file.is_some()
        && explicit_plugin_directives
        && toml_contains_table(&toml, "plugins")
    {
        anyhow::bail!(
            "@sandbox-config cannot be combined with @plugin directives when the copied config already contains a [plugins] table"
        );
    }

    let mut effective_plugins = plugin_config.clone();
    if recording_requested {
        anyhow::ensure!(
            !plugin_config
                .disable
                .iter()
                .any(|id| id == "bmux.recording"),
            "requested recording conflicts with explicitly disabled plugin 'bmux.recording'"
        );
        if should_write_generated_plugin_config {
            if !effective_plugins
                .enable
                .iter()
                .any(|id| id == "bmux.recording")
            {
                effective_plugins.enable.push("bmux.recording".to_owned());
            }
        } else {
            toml = enable_requested_recording(&toml)?;
        }
    }

    // Shell override. Avoid appending a duplicate [general] table when a copied
    // config already contains one; the copied config is authoritative in that
    // case.
    if let Some(shell) = shell
        && (sandbox_config_file.is_none() || !toml_contains_table(&toml, "general"))
    {
        write!(toml, "\n[general]\ndefault_shell = '{shell}'\n").unwrap();
    }

    // Plugin configuration — build disabled list. When an explicit sandbox
    // config is supplied, preserve its plugin/theme settings unless the
    // playbook also supplies explicit @plugin directives.
    if should_write_generated_plugin_config {
        for required in REQUIRED_SANDBOX_PLUGINS {
            anyhow::ensure!(
                !plugin_config.disable.iter().any(|id| id == required),
                "playbook typed services require plugin '{required}', but it was explicitly disabled; sandbox configuration was not overridden"
            );
        }
        let disabled = build_plugin_disabled_list(&effective_plugins, bundled_plugin_ids);
        let enabled = build_plugin_enabled_list(&effective_plugins);

        if !disabled.is_empty() || !enabled.is_empty() {
            toml.push_str("\n[plugins]\n");
            if !disabled.is_empty() {
                let quoted: Vec<String> = disabled.iter().map(|id| format!("'{id}'")).collect();
                let _ = writeln!(toml, "disabled = [{}]", quoted.join(", "));
            }
            if !enabled.is_empty() {
                let quoted: Vec<String> = enabled.iter().map(|id| format!("'{id}'")).collect();
                let _ = writeln!(toml, "enabled = [{}]", quoted.join(", "));
            }
        }
    }

    if let Some(profile) = input_profile {
        anyhow::ensure!(
            sandbox_config_file.is_none(),
            "input-profile cannot be combined with custom sandbox-config"
        );
        writeln!(
            toml,
            "\n[composition]\nactive_profile = '{}'",
            profile.name()
        )?;
    }
    std::fs::write(&config_path, toml)
        .with_context(|| format!("failed writing sandbox config {}", config_path.display()))
}

fn read_sandbox_config(source: Option<&Path>) -> Result<String> {
    let Some(source) = source else {
        return Ok(String::new());
    };
    let mut copied = std::fs::read_to_string(source)
        .with_context(|| format!("failed reading sandbox config source {}", source.display()))?;
    // Reject corrupt copied authority before generating or launching anything.
    let parsed: toml::Value =
        toml::from_str(&copied).context("invalid copied sandbox configuration")?;
    let _: bmux_config::BmuxConfig = parsed
        .clone()
        .try_into()
        .context("invalid copied sandbox configuration schema")?;
    if !copied.ends_with('\n') {
        copied.push('\n');
    }
    Ok(copied)
}

fn enable_requested_recording(source: &str) -> Result<String> {
    let mut config: toml::Value =
        toml::from_str(source).context("invalid copied sandbox configuration")?;
    let root = config
        .as_table_mut()
        .context("sandbox configuration must be a table")?;
    let plugins = root
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let parsed: bmux_config::PluginConfig = plugins
        .clone()
        .try_into()
        .context("invalid copied sandbox plugin configuration")?;
    anyhow::ensure!(
        !parsed.disabled.iter().any(|id| id == "bmux.recording"),
        "requested recording conflicts with explicitly disabled plugin 'bmux.recording' in sandbox-config"
    );
    let table = plugins
        .as_table_mut()
        .context("sandbox plugins must be a table")?;
    let enabled = table
        .entry("enabled")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let enabled = enabled
        .as_array_mut()
        .context("sandbox plugins.enabled must be an array")?;
    if !enabled
        .iter()
        .any(|id| id.as_str() == Some("bmux.recording"))
    {
        enabled.push(toml::Value::String("bmux.recording".to_owned()));
    }
    toml::to_string(&config).context("failed serializing recording sandbox configuration")
}

fn toml_contains_table(toml: &str, table: &str) -> bool {
    let expected = format!("[{table}]");
    let nested_prefix = format!("[{table}.");
    toml.lines()
        .map(str::trim)
        .any(|line| line == expected || line.starts_with(&nested_prefix))
}

// Product-owned requirements of the playbook's typed transport, not core
// defaults. Sessions allocates work; contexts/clients/tabs supply attachment
// grants, input and managed/real-attach navigation. Pane runtime and workspaces
// close their declared service requirements. Presentation stays opt-in.
const REQUIRED_SANDBOX_PLUGINS: [&str; 6] = [
    "bmux.sessions",
    "bmux.contexts",
    "bmux.clients",
    "bmux.tabs",
    "bmux.pane_runtime",
    "bmux.workspaces",
];

fn build_plugin_disabled_list(plugin_config: &PluginConfig, bundled_ids: &[String]) -> Vec<String> {
    let enabled = build_plugin_enabled_list(plugin_config);
    let mut disabled: Vec<String> = bundled_ids
        .iter()
        .filter(|id| !enabled.contains(id))
        .cloned()
        .collect();
    for id in &plugin_config.disable {
        if !disabled.contains(id) {
            disabled.push(id.clone());
        }
    }
    disabled.sort();
    disabled
}

fn build_plugin_enabled_list(plugin_config: &PluginConfig) -> Vec<String> {
    let mut enabled = plugin_config.enable.clone();
    for required in REQUIRED_SANDBOX_PLUGINS {
        if !plugin_config.disable.iter().any(|id| id == required)
            && !enabled.iter().any(|id| id == required)
        {
            enabled.push(required.to_owned());
        }
    }
    enabled.retain(|id| !plugin_config.disable.contains(id));
    enabled.sort();
    enabled.dedup();
    enabled
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
            if std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT").is_some() {
                return Err(fg_error.context(
                    "managed sandbox requires owned foreground child; daemon fallback disabled",
                ));
            }
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
    // This handle must own the actual server through readiness and cleanup,
    // not a launcher that silently backgrounds the process.
    cmd.arg("server").arg("start").arg("--foreground-internal");

    apply_sandbox_env(&mut cmd, paths, root_dir, env, env_mode);

    let ownership_marker = std::env::var_os("BMUX_PLAYBOOK_MANAGED_ROOT")
        .map(|root| PathBuf::from(root).join("sandbox-child-owned"));
    if let Some(marker) = ownership_marker.as_ref() {
        std::fs::write(marker, b"sandbox spawn pending; cleanup not acknowledged")?;
    }
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            if let Some(marker) = ownership_marker.as_ref() {
                std::fs::remove_file(marker)?;
            }
            return Err(error)
                .with_context(|| format!("failed spawning sandbox server {}", binary.display()));
        }
    };

    let mut handle = ServerHandle::Foreground {
        child,
        paths: paths.clone(),
        _stdout_log: stdout_log,
        stderr_log: stderr_log.clone(),
    };

    let managed = super::managed::ManagedRun::from_env()?;
    let ready = tokio::select! {
        ready = wait_for_server_ready(paths, timeout, handle.child_mut()) => ready,
        () = super::managed::cancellation(managed.as_ref()) => Err(anyhow::anyhow!("managed startup cancelled")),
    };
    match ready {
        Ok(()) => Ok(handle),
        Err(error) => {
            let excerpt = read_log_excerpt(&stderr_log);
            if let ServerHandle::Foreground { ref mut child, .. } = handle {
                let _ = child.kill();
                child
                    .wait()
                    .context("sandbox startup cleanup could not reap child")?;
                if let Some(marker) = ownership_marker.as_ref() {
                    std::fs::remove_file(marker)?;
                }
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

pub(crate) async fn wait_for_server_ready(
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
                if let Some(ref mut child) = child
                    && let Some(status) = child.try_wait()?
                {
                    anyhow::bail!("sandbox server exited before ready (status: {status})");
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

fn read_log_excerpt(path: &Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.lines().last().map(str::to_string))
        .filter(|line| !line.trim().is_empty())
        .unwrap_or_else(|| "<empty>".to_string())
}

impl ServerHandle {
    const fn child_mut(&mut self) -> Option<&mut std::process::Child> {
        match self {
            Self::Foreground { child, .. } => Some(child),
            Self::Daemon { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn private_root_allocation_retries_collisions_without_adopting_paths() {
        let parent = tempfile::tempdir().unwrap();
        let occupied = parent.path().join("occupied");
        create_private_directory(&occupied).unwrap();
        let marker = occupied.join("marker");
        std::fs::write(&marker, b"untouched").unwrap();
        let file = parent.path().join("file");
        std::fs::write(&file, b"also untouched").unwrap();
        let fresh = parent.path().join("fresh");
        let mut candidates = [occupied.clone(), file.clone(), fresh.clone()].into_iter();
        let allocated = allocate_private_root(|| candidates.next().unwrap()).unwrap();
        assert_eq!(allocated, fresh);
        assert_eq!(std::fs::read(marker).unwrap(), b"untouched");
        assert_eq!(std::fs::read(file).unwrap(), b"also untouched");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(allocated).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_root_allocation_never_follows_or_removes_collision_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("missing-target");
        let alias = parent.path().join("alias");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let fresh = parent.path().join("fresh");
        let mut candidates = [alias.clone(), fresh.clone()].into_iter();
        assert_eq!(
            allocate_private_root(|| candidates.next().unwrap()).unwrap(),
            fresh
        );
        assert_eq!(std::fs::read_link(alias).unwrap(), target);
        assert!(!target.exists());
    }

    #[test]
    fn private_root_allocation_bounds_collisions_and_propagates_other_errors() {
        let parent = tempfile::tempdir().unwrap();
        let occupied = parent.path().join("occupied");
        std::fs::write(&occupied, b"untouched").unwrap();
        let mut attempts = 0;
        let error = allocate_private_root(|| {
            attempts += 1;
            occupied.clone()
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(attempts, 32);
        assert_eq!(std::fs::read(occupied).unwrap(), b"untouched");
        let mut attempts = 0;
        let error = allocate_private_root(|| {
            attempts += 1;
            parent.path().join("missing-parent").join("root")
        })
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(attempts, 1);
    }

    #[test]
    fn private_root_allocation_is_exclusive_under_concurrency() {
        let parent = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(16);
        let roots = std::thread::scope(|scope| {
            let workers = (0..16)
                .map(|worker| {
                    let parent = parent.path();
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let mut first = true;
                        barrier.wait();
                        allocate_private_root(|| {
                            if std::mem::take(&mut first) {
                                parent.join("shared-first-candidate")
                            } else {
                                parent.join(format!("worker-{worker}"))
                            }
                        })
                        .unwrap()
                    })
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<std::collections::HashSet<_>>()
        });
        assert_eq!(roots.len(), 16);
        assert!(roots.iter().all(|root| root.is_dir()));
    }

    fn write_sandbox_config(
        paths: &ConfigPaths,
        shell: Option<&str>,
        input_profile: Option<super::super::types::InputProfile>,
        plugin_config: &PluginConfig,
        sandbox_config_file: Option<&Path>,
        bundled_plugin_ids: &[String],
    ) -> Result<()> {
        write_sandbox_config_with_options(
            paths,
            &SandboxStartOptions {
                shell,
                input_profile,
                plugin_config,
                recording_requested: false,
                startup_timeout: Duration::from_secs(1),
                env: &std::collections::BTreeMap::new(),
                env_mode: super::super::types::SandboxEnvMode::Clean,
                binary: None,
                sandbox_config_file,
                bundled_plugin_ids,
            },
        )
    }

    fn recording_config(
        paths: &ConfigPaths,
        plugins: &PluginConfig,
        source: Option<&Path>,
        bundled: &[String],
    ) -> Result<()> {
        write_sandbox_config_with_options(
            paths,
            &SandboxStartOptions {
                shell: None,
                input_profile: None,
                plugin_config: plugins,
                recording_requested: true,
                startup_timeout: Duration::from_secs(1),
                env: &std::collections::BTreeMap::new(),
                env_mode: super::super::types::SandboxEnvMode::Clean,
                binary: None,
                sandbox_config_file: source,
                bundled_plugin_ids: bundled,
            },
        )
    }

    fn test_paths(root: &Path) -> ConfigPaths {
        ConfigPaths::new(
            root.join("c"),
            root.join("r"),
            root.join("d"),
            root.join("s"),
        )
    }

    #[test]
    fn requested_recording_only_adds_recording_to_minimal_baseline() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        let mut bundled: Vec<String> = REQUIRED_SANDBOX_PLUGINS
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        bundled.extend(["bmux.recording".to_owned(), "bmux.tab_bar".to_owned()]);
        recording_config(&paths, &PluginConfig::default(), None, &bundled).unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(paths.config_file()).unwrap()).unwrap();
        let enabled = config["plugins"]["enabled"].as_array().unwrap();
        assert_eq!(enabled.len(), REQUIRED_SANDBOX_PLUGINS.len() + 1);
        assert!(
            enabled
                .iter()
                .any(|id| id.as_str() == Some("bmux.recording"))
        );
        for id in REQUIRED_SANDBOX_PLUGINS {
            assert!(enabled.iter().any(|value| value.as_str() == Some(id)));
        }
        let disabled = config["plugins"]["disabled"].as_array().unwrap();
        assert!(
            !disabled
                .iter()
                .any(|id| id.as_str() == Some("bmux.recording"))
        );
        assert!(
            disabled
                .iter()
                .any(|id| id.as_str() == Some("bmux.tab_bar"))
        );
    }

    #[test]
    fn requested_recording_rejects_explicit_dsl_disable_without_writing_config() {
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        let plugins = PluginConfig {
            enable: vec!["bmux.recording".to_owned()],
            disable: vec!["bmux.recording".to_owned()],
        };
        let error = recording_config(&paths, &plugins, None, &[]).unwrap_err();
        assert!(error.to_string().contains("explicitly disabled"));
        assert!(!paths.config_file().exists());
        assert_eq!(plugins.disable, vec!["bmux.recording"]);
    }

    #[test]
    fn requested_recording_preserves_copied_config_and_exact_selection() {
        let source = "[general]\ndefault_shell = 'sh'\n[plugins]\nenabled = ['bmux.finder']\ndisabled = ['bmux.cluster']\nsearch_paths = ['/selected/plugins']\n[plugins.sources.'bmux.recording']\nsource = 'dynamic'\nmanifest = '/selected/recording/plugin.toml'\n[plugins.settings.'bmux.recording']\ncustom = 'retained'\n";
        let mut original: toml::Value = toml::from_str(source).unwrap();
        let actual: toml::Value =
            toml::from_str(&enable_requested_recording(source).unwrap()).unwrap();
        original["plugins"]["enabled"]
            .as_array_mut()
            .unwrap()
            .push(toml::Value::String("bmux.recording".to_owned()));
        assert_eq!(actual, original);
        let root = tempfile::tempdir().unwrap();
        let paths = test_paths(root.path());
        let source_path = root.path().join("source.toml");
        std::fs::write(&source_path, source).unwrap();
        recording_config(&paths, &PluginConfig::default(), Some(&source_path), &[]).unwrap();
        let written: toml::Value =
            toml::from_str(&std::fs::read_to_string(paths.config_file()).unwrap()).unwrap();
        assert_eq!(written, original);
        assert_eq!(std::fs::read_to_string(source_path).unwrap(), source);
        assert_eq!(
            enable_requested_recording(&enable_requested_recording(source).unwrap()).unwrap(),
            enable_requested_recording(source).unwrap()
        );
    }

    #[test]
    fn requested_recording_rejects_copied_disable_and_corrupt_config() {
        for source in [
            "[plugins]\ndisabled = ['bmux.recording']\n",
            "[plugins]\nenabled = ['bmux.recording']\ndisabled = ['bmux.recording']\n",
            "[plugins]\nenabled = 42\n",
            "[general]\ndefault_shell = 42\n",
            "[plugins.sources.'bmux.recording']\nsource = 'unknown'\n",
            "[plugins]\ndisabled = ['bmux.recording'\n",
        ] {
            let root = tempfile::tempdir().unwrap();
            let paths = test_paths(root.path());
            let source_path = root.path().join("source.toml");
            std::fs::write(&source_path, source).unwrap();
            assert!(
                recording_config(&paths, &PluginConfig::default(), Some(&source_path), &[])
                    .is_err()
            );
            assert!(!paths.config_file().exists());
            assert_eq!(std::fs::read_to_string(source_path).unwrap(), source);
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_sandbox_creation_does_not_repair_or_follow_existing_paths() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("sandbox");
        create_private_directory(&directory).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(create_private_directory(&directory).is_err());
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&directory, &alias).unwrap();
        assert!(create_private_directory(&alias).is_err());
        assert!(
            std::fs::symlink_metadata(alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    fn test_timestamp_millis() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }

    #[test]
    fn shutdown_retains_requested_or_failed_sandbox_diagnostics() {
        assert!(!retain_sandbox_after_shutdown(false, true));
        assert!(retain_sandbox_after_shutdown(true, true));
        assert!(retain_sandbox_after_shutdown(false, false));
        assert!(retain_sandbox_after_shutdown(true, false));
    }
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    fn command_env_value(command: &ProcessCommand, key: &str) -> Option<std::ffi::OsString> {
        command.get_envs().find_map(|(name, value)| {
            if name == OsStr::new(key) {
                value.map(std::ffi::OsStr::to_os_string)
            } else {
                None
            }
        })
    }

    #[test]
    fn generated_input_profile_is_isolated_and_conflicts_with_custom_config() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        write_sandbox_config(
            &paths,
            Some("sh"),
            Some(super::super::types::InputProfile::TmuxCompat),
            &PluginConfig::default(),
            None,
            &[],
        )
        .unwrap();
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(paths.config_file()).unwrap()).unwrap();
        assert_eq!(
            value["composition"]["active_profile"].as_str(),
            Some("tmux_compat")
        );
        let source = root.path().join("custom.toml");
        std::fs::write(&source, "").unwrap();
        let error = write_sandbox_config(
            &paths,
            None,
            Some(super::super::types::InputProfile::Vim),
            &PluginConfig::default(),
            Some(&source),
            &[],
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot be combined"));
    }

    #[test]
    fn default_sandbox_keeps_typed_session_providers_without_cosmetic_plugins() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(
            root.path().join("c"),
            root.path().join("r"),
            root.path().join("d"),
            root.path().join("s"),
        );
        let mut bundled: Vec<String> = REQUIRED_SANDBOX_PLUGINS
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        bundled.push("bmux.tab_bar".into());
        write_sandbox_config(&paths, None, None, &PluginConfig::default(), None, &bundled).unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(paths.config_file()).unwrap()).unwrap();
        let enabled = config["plugins"]["enabled"].as_array().unwrap();
        let disabled = config["plugins"]["disabled"].as_array().unwrap();
        for required in REQUIRED_SANDBOX_PLUGINS {
            assert!(enabled.iter().any(|id| id.as_str() == Some(required)));
            assert!(!disabled.iter().any(|id| id.as_str() == Some(required)));
        }
        assert!(
            disabled
                .iter()
                .any(|id| id.as_str() == Some("bmux.tab_bar"))
        );
        assert_eq!(enabled.len(), REQUIRED_SANDBOX_PLUGINS.len());
    }

    #[test]
    fn explicitly_disabled_typed_provider_is_preserved_and_rejected_before_config_write() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(
            root.path().join("c"),
            root.path().join("r"),
            root.path().join("d"),
            root.path().join("s"),
        );
        let plugins = PluginConfig {
            enable: vec![],
            disable: vec!["bmux.sessions".into()],
        };
        assert!(!build_plugin_enabled_list(&plugins).contains(&"bmux.sessions".into()));
        assert!(build_plugin_disabled_list(&plugins, &[]).contains(&"bmux.sessions".into()));
        let error = write_sandbox_config(&paths, None, None, &plugins, None, &[]).unwrap_err();
        assert!(error.to_string().contains("require plugin 'bmux.sessions'"));
        assert!(error.to_string().contains("explicitly disabled"));
        assert!(!paths.config_file().exists());
        assert_eq!(plugins.disable, vec!["bmux.sessions"]);
    }

    #[test]
    fn copied_sandbox_config_preserves_theme_settings_without_default_plugin_disable() {
        let root = std::env::temp_dir().join(format!(
            "bmux-playbook-config-preserve-test-{}-{}",
            std::process::id(),
            test_timestamp_millis()
        ));
        let paths = ConfigPaths::new(
            root.join("c"),
            root.join("r"),
            root.join("d"),
            root.join("s"),
        );
        let source = root.join("source.toml");
        std::fs::create_dir_all(&root).expect("test root should be created");
        std::fs::write(
            &source,
            "[plugins.settings.\"bmux.theme\"]\ncomponent_themes = [\"performance\", \"pong\"]\n",
        )
        .expect("source config should be written");

        write_sandbox_config(
            &paths,
            None,
            None,
            &PluginConfig::default(),
            Some(&source),
            &["bmux.theme".to_string()],
        )
        .expect("sandbox config should be written");

        let written = std::fs::read_to_string(paths.config_file()).expect("config should read");
        assert!(written.contains("component_themes"));
        assert!(!written.contains("disabled"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn copied_sandbox_config_rejects_duplicate_plugin_table_generation() {
        let root = std::env::temp_dir().join(format!(
            "bmux-playbook-config-duplicate-test-{}-{}",
            std::process::id(),
            test_timestamp_millis()
        ));
        let paths = ConfigPaths::new(
            root.join("c"),
            root.join("r"),
            root.join("d"),
            root.join("s"),
        );
        let source = root.join("source.toml");
        std::fs::create_dir_all(&root).expect("test root should be created");
        std::fs::write(&source, "[plugins]\nenabled = [\"bmux.theme\"]\n")
            .expect("source config should be written");

        let error = write_sandbox_config(
            &paths,
            None,
            None,
            &PluginConfig {
                enable: vec!["bmux.theme".to_string()],
                disable: Vec::new(),
            },
            Some(&source),
            &["bmux.theme".to_string()],
        )
        .expect_err("duplicate [plugins] generation should fail");

        assert!(
            error
                .to_string()
                .contains("@sandbox-config cannot be combined")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_sandbox_env_sets_log_dir_inside_sandbox_root() {
        let root =
            std::env::temp_dir().join(format!("bmux-playbook-env-test-{}", std::process::id()));
        let paths = ConfigPaths::new(
            root.join("c"),
            root.join("r"),
            root.join("d"),
            root.join("s"),
        );
        let mut command = ProcessCommand::new("sh");

        apply_sandbox_env(
            &mut command,
            &paths,
            &root,
            &BTreeMap::new(),
            super::super::types::SandboxEnvMode::Inherit,
        );

        assert_eq!(
            command_env_value(&command, "BMUX_LOG_DIR"),
            Some(root.join("logs").into_os_string())
        );
    }
}
