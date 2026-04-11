use anyhow::{Context, Result};
use bmux_cli_schema::SandboxEnvModeArg;
use bmux_config::ConfigPaths;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SANDBOX_PREFIX: &str = "bmux-sbx-";
const CLEANUP_MIN_AGE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct CleanupEntry {
    path: String,
    age_secs: u64,
    removed: bool,
}

#[derive(Debug, Clone)]
struct SandboxPaths {
    root_dir: PathBuf,
    config_home: PathBuf,
    data_home: PathBuf,
    runtime_dir: PathBuf,
    state_dir: PathBuf,
    log_dir: PathBuf,
    tmp_dir: PathBuf,
    home_dir: PathBuf,
    config_paths: ConfigPaths,
}

impl SandboxPaths {
    fn new(name: Option<&str>) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let suffix = name
            .map(sanitize_component)
            .filter(|value| !value.is_empty())
            .map_or_else(String::new, |value| format!("-{value}"));
        let root_dir = std::env::temp_dir().join(format!(
            "{SANDBOX_PREFIX}{nanos:x}-{}{}",
            std::process::id(),
            suffix
        ));

        let config_home = root_dir.join("config");
        let data_home = root_dir.join("data");
        let runtime_dir = root_dir.join("runtime");
        let state_dir = root_dir.join("state");
        let log_dir = root_dir.join("logs");
        let tmp_dir = root_dir.join("tmp");
        let home_dir = root_dir.join("home");

        let config_paths = ConfigPaths::new(
            config_home.join("bmux"),
            runtime_dir.clone(),
            data_home.join("bmux"),
            state_dir.clone(),
        );

        Self {
            root_dir,
            config_home,
            data_home,
            runtime_dir,
            state_dir,
            log_dir,
            tmp_dir,
            home_dir,
            config_paths,
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_home)
            .with_context(|| format!("failed creating {}", self.config_home.display()))?;
        std::fs::create_dir_all(&self.data_home)
            .with_context(|| format!("failed creating {}", self.data_home.display()))?;
        std::fs::create_dir_all(&self.runtime_dir)
            .with_context(|| format!("failed creating {}", self.runtime_dir.display()))?;
        std::fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("failed creating {}", self.state_dir.display()))?;
        std::fs::create_dir_all(&self.log_dir)
            .with_context(|| format!("failed creating {}", self.log_dir.display()))?;
        std::fs::create_dir_all(&self.tmp_dir)
            .with_context(|| format!("failed creating {}", self.tmp_dir.display()))?;
        std::fs::create_dir_all(&self.home_dir)
            .with_context(|| format!("failed creating {}", self.home_dir.display()))?;
        std::fs::create_dir_all(&self.config_paths.config_dir).with_context(|| {
            format!("failed creating {}", self.config_paths.config_dir.display())
        })?;
        std::fs::create_dir_all(&self.config_paths.data_dir)
            .with_context(|| format!("failed creating {}", self.config_paths.data_dir.display()))?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct SandboxRunReport {
    sandbox_root: String,
    bmux_bin: String,
    env_mode: &'static str,
    keep_requested: bool,
    kept: bool,
    exit_code: u8,
}

pub(super) async fn run_sandbox_run(
    bmux_bin: Option<&str>,
    env_mode: SandboxEnvModeArg,
    keep: bool,
    json: bool,
    name: Option<&str>,
    command_args: &[String],
) -> Result<u8> {
    let sandbox = SandboxPaths::new(name);
    sandbox.ensure_dirs()?;
    write_pid_marker(&sandbox.root_dir)?;

    let binary = resolve_bmux_binary(bmux_bin)?;
    let mut command = ProcessCommand::new(&binary);
    command.args(command_args);
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    apply_sandbox_env(&mut command, &sandbox, env_mode);

    let status = command
        .status()
        .with_context(|| format!("failed running sandbox command with {}", binary.display()))?;

    let exit_code = status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1);
    let keep_on_failure = !status.success();
    let kept = keep || keep_on_failure;

    if json {
        let report = SandboxRunReport {
            sandbox_root: sandbox.root_dir.to_string_lossy().to_string(),
            bmux_bin: binary.to_string_lossy().to_string(),
            env_mode: sandbox_env_mode_name(env_mode),
            keep_requested: keep,
            kept,
            exit_code,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if kept {
        if keep_on_failure {
            eprintln!(
                "sandbox command exited with {exit_code}; keeping sandbox at {}",
                sandbox.root_dir.display()
            );
        } else {
            println!("kept sandbox at {}", sandbox.root_dir.display());
        }
        eprintln!("sandbox logs: {}", sandbox.log_dir.display());
    }

    if !kept {
        let _ = std::fs::remove_dir_all(&sandbox.root_dir);
    }

    Ok(exit_code)
}

pub(super) fn run_sandbox_cleanup(dry_run: bool, json: bool) -> Result<u8> {
    let (scanned, entries) = cleanup_orphaned_sandboxes(dry_run);
    let orphaned = entries.len();

    if json {
        let report = serde_json::json!({
            "scanned": scanned,
            "orphaned": orphaned,
            "dry_run": dry_run,
            "entries": entries,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else if orphaned > 0 {
        for entry in &entries {
            let status = if entry.removed { "removed" } else { "found" };
            println!("  {status}: {} (age: {}s)", entry.path, entry.age_secs);
        }
        if dry_run {
            println!("{orphaned} orphaned sandbox(es) found (dry run, not removed)");
        } else {
            let removed = entries.iter().filter(|entry| entry.removed).count();
            println!("{removed} orphaned sandbox(es) removed");
        }
    } else {
        println!("no orphaned sandboxes found ({scanned} scanned)");
    }

    Ok(0)
}

fn cleanup_orphaned_sandboxes(dry_run: bool) -> (usize, Vec<CleanupEntry>) {
    let temp_dir = std::env::temp_dir();
    let now = SystemTime::now();
    let mut scanned = 0;
    let mut entries = Vec::new();

    let Ok(dir_entries) = std::fs::read_dir(temp_dir) else {
        return (0, entries);
    };

    for dir_entry in dir_entries {
        let Ok(dir_entry) = dir_entry else { continue };
        let name = dir_entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(SANDBOX_PREFIX) || !dir_entry.path().is_dir() {
            continue;
        }

        scanned += 1;
        let age = dir_entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default();
        if age < CLEANUP_MIN_AGE {
            continue;
        }

        let root_path = dir_entry.path();
        if sandbox_process_alive(&root_path) || sandbox_socket_alive(&root_path) {
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

fn write_pid_marker(root_dir: &Path) -> Result<()> {
    let marker_path = root_dir.join("sandbox.pid");
    std::fs::write(&marker_path, std::process::id().to_string())
        .with_context(|| format!("failed writing {}", marker_path.display()))
}

fn apply_sandbox_env(
    command: &mut ProcessCommand,
    sandbox: &SandboxPaths,
    env_mode: SandboxEnvModeArg,
) {
    if matches!(env_mode, SandboxEnvModeArg::Clean) {
        command.env_clear();
    }

    command
        .env("BMUX_CONFIG_DIR", &sandbox.config_paths.config_dir)
        .env("BMUX_RUNTIME_DIR", &sandbox.config_paths.runtime_dir)
        .env("BMUX_DATA_DIR", &sandbox.config_paths.data_dir)
        .env("BMUX_STATE_DIR", &sandbox.config_paths.state_dir)
        .env("BMUX_LOG_DIR", &sandbox.log_dir)
        .env("XDG_CONFIG_HOME", &sandbox.config_home)
        .env("XDG_DATA_HOME", &sandbox.data_home)
        .env("XDG_RUNTIME_DIR", &sandbox.runtime_dir)
        .env("TMPDIR", &sandbox.tmp_dir)
        .env("HOME", &sandbox.home_dir)
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8");

    if matches!(env_mode, SandboxEnvModeArg::Clean) {
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        if let Ok(user) = std::env::var("USER") {
            command.env("USER", user);
        }
        if let Ok(shell) = std::env::var("SHELL") {
            command.env("SHELL", shell);
        }
    }
}

fn resolve_bmux_binary(bmux_bin: Option<&str>) -> Result<PathBuf> {
    bmux_bin.map_or_else(
        || std::env::current_exe().context("failed resolving current bmux executable"),
        resolve_explicit_binary,
    )
}

fn resolve_explicit_binary(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--bmux-bin requires a non-empty path");
    }
    let candidate = PathBuf::from(trimmed);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        std::env::current_dir()
            .context("failed resolving current directory for --bmux-bin")?
            .join(candidate)
    };
    if !resolved.exists() {
        anyhow::bail!("--bmux-bin path does not exist: {}", resolved.display());
    }
    Ok(resolved)
}

const fn sandbox_env_mode_name(mode: SandboxEnvModeArg) -> &'static str {
    match mode {
        SandboxEnvModeArg::Clean => "clean",
        SandboxEnvModeArg::Inherit => "inherit",
    }
}

fn sanitize_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn sandbox_process_alive(root_dir: &Path) -> bool {
    let marker_path = root_dir.join("sandbox.pid");
    std::fs::read_to_string(marker_path)
        .ok()
        .and_then(|contents| contents.trim().parse::<u32>().ok())
        .is_some_and(is_pid_alive)
}

fn sandbox_socket_alive(root_dir: &Path) -> bool {
    #[cfg(unix)]
    {
        let socket_path = root_dir.join("runtime").join("server.sock");
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
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    {
        ProcessCommand::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
    use super::{SandboxEnvModeArg, SandboxPaths, apply_sandbox_env, sanitize_component};

    fn env_value(command: &std::process::Command, key: &str) -> Option<std::ffi::OsString> {
        command.get_envs().find_map(|(name, value)| {
            if name == std::ffi::OsStr::new(key) {
                value.map(std::ffi::OsStr::to_os_string)
            } else {
                None
            }
        })
    }

    #[test]
    fn sanitize_component_rewrites_non_alnum() {
        assert_eq!(sanitize_component("my sandbox/test"), "my-sandbox-test");
    }

    #[test]
    fn sandbox_env_sets_clean_defaults() {
        let sandbox = SandboxPaths::new(None);
        sandbox
            .ensure_dirs()
            .expect("sandbox dirs should be created");
        let mut command = std::process::Command::new("sh");
        apply_sandbox_env(&mut command, &sandbox, SandboxEnvModeArg::Clean);
        let expected_log_dir = sandbox.log_dir.clone();
        let root_dir = sandbox.root_dir.clone();

        assert_eq!(
            env_value(&command, "BMUX_LOG_DIR"),
            Some(expected_log_dir.into_os_string())
        );
        assert_eq!(
            env_value(&command, "TERM"),
            Some(std::ffi::OsString::from("xterm-256color"))
        );

        let _ = std::fs::remove_dir_all(root_dir);
    }

    #[test]
    fn sandbox_env_sets_inherit_defaults() {
        let sandbox = SandboxPaths::new(None);
        sandbox
            .ensure_dirs()
            .expect("sandbox dirs should be created");
        let mut command = std::process::Command::new("sh");
        apply_sandbox_env(&mut command, &sandbox, SandboxEnvModeArg::Inherit);
        let expected_runtime_dir = sandbox.runtime_dir.clone();
        let root_dir = sandbox.root_dir.clone();

        assert_eq!(
            env_value(&command, "XDG_RUNTIME_DIR"),
            Some(expected_runtime_dir.into_os_string())
        );

        let _ = std::fs::remove_dir_all(root_dir);
    }
}
