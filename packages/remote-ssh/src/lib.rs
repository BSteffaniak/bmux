//! SSH targets and remote bmux execution shared by the CLI's target proxy and
//! the connections administration commands. Transport-neutral: no session,
//! pairing or trust policy lives here.
use anyhow::{Context, Result};
use bmux_config::RemoteServerStartMode;
use std::{ffi::OsString, path::PathBuf, process::Command as ProcessCommand};

/// A remote host reached by running `bmux` over the system `ssh` client.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub label: String,
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
    pub known_hosts_file: Option<PathBuf>,
    pub strict_host_key_checking: bool,
    pub jump: Option<String>,
    pub remote_bmux_path: String,
    pub connect_timeout_ms: u64,
    pub server_start_mode: RemoteServerStartMode,
}

/// Parse an inline `[ssh://][user@]host[:port]` target with default options.
///
/// # Errors
/// Rejects a missing host or an invalid port.
pub fn parse_inline_ssh_target(target: &str) -> Result<SshTarget> {
    let mut raw = target.trim();
    if let Some(without_scheme) = raw.strip_prefix("ssh://") {
        raw = without_scheme;
    }
    let (user, host_port) = if let Some((user, rest)) = raw.split_once('@') {
        (Some(user.to_string()), rest)
    } else {
        (None, raw)
    };
    let (host, port) = if let Some((host, port_raw)) = host_port.rsplit_once(':') {
        if port_raw.is_empty() {
            (host_port.to_string(), None)
        } else {
            let parsed = port_raw
                .parse::<u16>()
                .with_context(|| format!("invalid SSH port in target '{target}'"))?;
            (host.to_string(), Some(parsed))
        }
    } else {
        (host_port.to_string(), None)
    };
    if host.trim().is_empty() {
        anyhow::bail!("target must include a host");
    }
    Ok(SshTarget {
        label: target.to_string(),
        host,
        user,
        port,
        identity_file: None,
        known_hosts_file: None,
        strict_host_key_checking: true,
        jump: None,
        remote_bmux_path: "bmux".to_string(),
        connect_timeout_ms: 8_000,
        server_start_mode: RemoteServerStartMode::Auto,
    })
}

/// Build `ssh … <remote bmux> <args>` with the target's connection options.
#[must_use]
pub fn build_ssh_command(target: &SshTarget, args: &[OsString], force_tty: bool) -> ProcessCommand {
    let mut command = ProcessCommand::new("ssh");
    if force_tty {
        command.arg("-t");
    }
    if let Some(port) = target.port {
        command.arg("-p");
        command.arg(port.to_string());
    }
    if let Some(path) = target.identity_file.as_ref() {
        command.arg("-i");
        command.arg(path);
    }
    if let Some(jump) = target.jump.as_ref() {
        command.arg("-J");
        command.arg(jump);
    }
    command.arg("-o");
    command.arg(format!(
        "StrictHostKeyChecking={}",
        if target.strict_host_key_checking {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(known_hosts) = target.known_hosts_file.as_ref() {
        command.arg("-o");
        command.arg(format!("UserKnownHostsFile={}", known_hosts.display()));
    }
    command.arg("-o");
    let timeout_secs = (target.connect_timeout_ms.saturating_add(999)) / 1000;
    command.arg(format!("ConnectTimeout={timeout_secs}"));
    command.arg("-o");
    command.arg("ServerAliveInterval=15");
    command.arg("-o");
    command.arg("ServerAliveCountMax=3");
    command.arg("-o");
    command.arg("BatchMode=yes");
    let destination = target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    );
    command.arg(destination);
    command.arg(&target.remote_bmux_path);
    command.args(args);
    command
}

/// Run remote bmux; prints its stdout unless a TTY is forced.
///
/// # Errors
/// Returns a mapped SSH failure or a launch error.
pub fn run_ssh_bmux_command(target: &SshTarget, args: &[OsString], force_tty: bool) -> Result<u8> {
    run_ssh_bmux_command_inner(target, args, force_tty, true)
}

/// Run remote bmux without echoing its stdout.
///
/// # Errors
/// Returns a mapped SSH failure or a launch error.
pub fn run_ssh_bmux_command_silent(
    target: &SshTarget,
    args: &[OsString],
    force_tty: bool,
) -> Result<u8> {
    run_ssh_bmux_command_inner(target, args, force_tty, false)
}

fn run_ssh_bmux_command_inner(
    target: &SshTarget,
    args: &[OsString],
    force_tty: bool,
    print_stdout: bool,
) -> Result<u8> {
    if !force_tty {
        let output = build_ssh_command(target, args, false)
            .output()
            .with_context(|| format!("failed executing ssh target {}", target.label))?;
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if print_stdout && !stdout.trim().is_empty() {
                print!("{stdout}");
            }
            return Ok(0);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(map_ssh_execution_error(target, stderr.trim()));
    }
    let mut command = build_ssh_command(target, args, force_tty);
    let status = command
        .status()
        .with_context(|| format!("failed executing ssh target {}", target.label))?;
    Ok(exit_code_from_status(status))
}

/// A process exit status as a bmux exit code (1 when unavailable).
#[must_use]
pub fn exit_code_from_status(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1)
}

/// Translate common ssh stderr diagnostics into actionable errors.
#[must_use]
pub fn map_ssh_execution_error(target: &SshTarget, stderr: &str) -> anyhow::Error {
    if stderr.contains("Host key verification failed") {
        return anyhow::anyhow!(
            "ssh host key verification failed for '{}'. verify known_hosts or set known_hosts_file",
            target.label
        );
    }
    if stderr.contains("Permission denied") {
        return anyhow::anyhow!(
            "ssh authentication failed for '{}'. check user/identity_file and remote access",
            target.label
        );
    }
    if stderr.contains("Could not resolve hostname") {
        return anyhow::anyhow!(
            "ssh target '{}' hostname could not be resolved",
            target.label
        );
    }
    if stderr.contains("Connection timed out") || stderr.contains("Operation timed out") {
        return anyhow::anyhow!("ssh connection to '{}' timed out", target.label);
    }
    if stderr.is_empty() {
        return anyhow::anyhow!("ssh command failed for '{}'", target.label);
    }
    anyhow::anyhow!("ssh command failed for '{}': {stderr}", target.label)
}

/// `user@host`, or `host` when no user is configured.
#[must_use]
pub fn ssh_destination(target: &SshTarget) -> String {
    target.user.as_ref().map_or_else(
        || target.host.clone(),
        |user| format!("{user}@{}", target.host),
    )
}

/// Split `host[:port]`, using `default_port` when the port is omitted.
///
/// # Errors
/// Rejects an empty host or an invalid port.
pub fn parse_host_port_with_default(value: &str, default_port: u16) -> Result<(String, u16)> {
    let raw = value.trim();
    if let Some((host, port_raw)) = raw.rsplit_once(':') {
        if port_raw.is_empty() {
            return Ok((raw.to_string(), default_port));
        }
        let port = port_raw
            .parse::<u16>()
            .with_context(|| format!("invalid port in '{value}'"))?;
        if host.trim().is_empty() {
            anyhow::bail!("host is required");
        }
        return Ok((host.to_string(), port));
    }
    if raw.is_empty() {
        anyhow::bail!("host is required");
    }
    Ok((raw.to_string(), default_port))
}

#[cfg(test)]
mod tests {
    use super::{map_ssh_execution_error, parse_inline_ssh_target};

    #[test]
    fn map_ssh_execution_error_highlights_auth_failures() {
        let target = parse_inline_ssh_target("ssh://deploy@example.com:2222").unwrap();
        let error = map_ssh_execution_error(&target, "Permission denied (publickey)");
        assert!(error.to_string().contains("authentication failed"));
    }

    #[test]
    fn parse_inline_ssh_target_reads_user_host_and_port() {
        let target = parse_inline_ssh_target("ssh://deploy@example.com:2222").unwrap();
        assert_eq!(target.user.as_deref(), Some("deploy"));
        assert_eq!(
            (target.host.as_str(), target.port),
            ("example.com", Some(2222))
        );
        assert!(parse_inline_ssh_target("ssh://deploy@").is_err());
    }
}
