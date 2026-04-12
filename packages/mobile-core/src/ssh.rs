use crate::error::{MobileCoreError, Result};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
    pub strict_host_key_checking: bool,
    pub known_hosts_file: Option<PathBuf>,
    pub identity_file: Option<PathBuf>,
}

pub trait SshBackend: Send + Sync {
    /// Open an SSH transport using embedded Rust implementation.
    ///
    /// # Errors
    ///
    /// Returns connection-level errors from the backend implementation.
    fn open(&self, target: &SshTarget) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct EmbeddedSshBackend {
    connect_timeout: Duration,
}

impl Default for EmbeddedSshBackend {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
        }
    }
}

impl EmbeddedSshBackend {
    #[must_use]
    pub const fn new(connect_timeout: Duration) -> Self {
        Self { connect_timeout }
    }
}

impl SshBackend for EmbeddedSshBackend {
    fn open(&self, target: &SshTarget) -> Result<()> {
        let username = target
            .user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .ok_or_else(|| {
                MobileCoreError::SshConnectionFailed(
                    "missing ssh username; provide user@host or set USER".to_string(),
                )
            })?;

        let endpoint = format!("{}:{}", target.host, target.port);
        let address = endpoint
            .to_socket_addrs()
            .map_err(|error| {
                MobileCoreError::SshConnectionFailed(format!(
                    "failed resolving SSH target '{endpoint}': {error}"
                ))
            })?
            .next()
            .ok_or_else(|| {
                MobileCoreError::SshConnectionFailed(format!(
                    "no resolved addresses for SSH target '{endpoint}'"
                ))
            })?;

        let stream =
            TcpStream::connect_timeout(&address, self.connect_timeout).map_err(|error| {
                MobileCoreError::SshConnectionFailed(format!(
                    "failed connecting to SSH target '{endpoint}': {error}"
                ))
            })?;
        stream
            .set_read_timeout(Some(self.connect_timeout))
            .map_err(|error| {
                MobileCoreError::SshConnectionFailed(format!(
                    "failed setting SSH read timeout for '{endpoint}': {error}"
                ))
            })?;
        stream
            .set_write_timeout(Some(self.connect_timeout))
            .map_err(|error| {
                MobileCoreError::SshConnectionFailed(format!(
                    "failed setting SSH write timeout for '{endpoint}': {error}"
                ))
            })?;

        let mut session = ssh2::Session::new().map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!("failed creating SSH session: {error}"))
        })?;
        session.set_tcp_stream(stream);
        session.handshake().map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "SSH handshake failed for '{endpoint}': {error}"
            ))
        })?;

        verify_host_key(&session, target)?;

        authenticate_with_fallback(&session, &username, target)?;

        let mut channel = session.channel_session().map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "failed opening SSH channel for '{endpoint}': {error}"
            ))
        })?;
        channel.exec("true").map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "failed validating SSH exec capability on '{endpoint}': {error}"
            ))
        })?;
        channel.wait_close().map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "failed closing SSH validation channel on '{endpoint}': {error}"
            ))
        })?;

        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct MockSshBackend;

impl SshBackend for MockSshBackend {
    fn open(&self, _target: &SshTarget) -> Result<()> {
        Ok(())
    }
}

/// Parse an SSH target from either `ssh://user@host:port` or `user@host:port`.
///
/// # Errors
///
/// Returns [`MobileCoreError::InvalidTarget`] when host/port format is invalid.
pub fn parse_ssh_target(value: &str) -> Result<SshTarget> {
    let mut raw = value.trim();
    if let Some(without_scheme) = raw.strip_prefix("ssh://") {
        raw = without_scheme;
    }

    if raw.is_empty() {
        return Err(MobileCoreError::InvalidTarget(
            "ssh target cannot be empty".to_string(),
        ));
    }

    let (host_input, query) = match raw.split_once('?') {
        Some((left, right)) => (left, Some(right)),
        None => (raw, None),
    };

    let (user, host_port) = if let Some((user, rest)) = host_input.split_once('@') {
        let normalized_user = user.trim();
        if normalized_user.is_empty() {
            return Err(MobileCoreError::InvalidTarget(
                "ssh user cannot be empty when @ is present".to_string(),
            ));
        }
        (Some(normalized_user.to_string()), rest)
    } else {
        (None, raw)
    };

    let (host, port) = if let Some((host, port_raw)) = host_port.rsplit_once(':') {
        if host.trim().is_empty() {
            return Err(MobileCoreError::InvalidTarget(
                "ssh host cannot be empty".to_string(),
            ));
        }
        let parsed_port = port_raw.trim().parse::<u16>().map_err(|_| {
            MobileCoreError::InvalidTarget(format!("invalid ssh port in target '{value}'"))
        })?;
        (host.trim().to_string(), parsed_port)
    } else {
        let normalized_host = host_port.trim();
        if normalized_host.is_empty() {
            return Err(MobileCoreError::InvalidTarget(
                "ssh host cannot be empty".to_string(),
            ));
        }
        (normalized_host.to_string(), 22)
    };

    let mut strict_host_key_checking = true;
    let mut known_hosts_file = None;
    let mut identity_file = None;
    if let Some(query_value) = query {
        for pair in query_value.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = match pair.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (pair.trim(), ""),
            };
            match key {
                "strict" | "strict_host_key_checking" => {
                    strict_host_key_checking = parse_bool_flag(value)?;
                }
                "known_hosts" | "known_hosts_file" => {
                    if !value.is_empty() {
                        known_hosts_file = Some(PathBuf::from(value));
                    }
                }
                "identity" | "identity_file" => {
                    if !value.is_empty() {
                        identity_file = Some(PathBuf::from(value));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(SshTarget {
        user,
        host,
        port,
        strict_host_key_checking,
        known_hosts_file,
        identity_file,
    })
}

fn parse_bool_flag(value: &str) -> Result<bool> {
    match value {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(MobileCoreError::InvalidTarget(format!(
            "invalid boolean flag value '{value}'"
        ))),
    }
}

fn authenticate_with_fallback(
    session: &ssh2::Session,
    username: &str,
    target: &SshTarget,
) -> Result<()> {
    let agent_error = if let Err(error) = session.userauth_agent(username) {
        Some(error.to_string())
    } else {
        None
    };
    if session.authenticated() {
        return Ok(());
    }

    if let Some(path) = target.identity_file.as_ref() {
        attempt_identity_file_auth(session, username, path)?;
        if session.authenticated() {
            return Ok(());
        }
    } else {
        for candidate in default_identity_files() {
            if !candidate.exists() {
                continue;
            }
            if attempt_identity_file_auth(session, username, &candidate).is_ok()
                && session.authenticated()
            {
                return Ok(());
            }
        }
    }

    let agent_detail = agent_error.unwrap_or_else(|| "agent auth unavailable".to_string());
    Err(MobileCoreError::SshConnectionFailed(format!(
        "SSH auth failed for '{username}@{}:{}' ({agent_detail})",
        target.host, target.port
    )))
}

fn attempt_identity_file_auth(session: &ssh2::Session, username: &str, path: &Path) -> Result<()> {
    session
        .userauth_pubkey_file(username, None, path, None)
        .map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "SSH key auth failed with '{}': {error}",
                path.display()
            ))
        })
}

fn default_identity_files() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let root = PathBuf::from(home).join(".ssh");
    vec![root.join("id_ed25519"), root.join("id_rsa")]
}

fn verify_host_key(session: &ssh2::Session, target: &SshTarget) -> Result<()> {
    if !target.strict_host_key_checking {
        return Ok(());
    }

    let (host_key, _) = session.host_key().ok_or_else(|| {
        MobileCoreError::SshConnectionFailed("SSH server did not provide a host key".to_string())
    })?;

    let mut known_hosts = session.known_hosts().map_err(|error| {
        MobileCoreError::SshConnectionFailed(format!(
            "failed creating known_hosts context: {error}"
        ))
    })?;

    let known_hosts_path = resolve_known_hosts_path(target)?;
    known_hosts
        .read_file(&known_hosts_path, ssh2::KnownHostFileKind::OpenSSH)
        .map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "failed reading known_hosts '{}': {error}",
                known_hosts_path.display()
            ))
        })?;

    match known_hosts.check_port(&target.host, target.port, host_key) {
        ssh2::CheckResult::Match => Ok(()),
        ssh2::CheckResult::Mismatch => Err(MobileCoreError::SshConnectionFailed(format!(
            "host key mismatch for '{}:{}'",
            target.host, target.port
        ))),
        ssh2::CheckResult::NotFound => Err(MobileCoreError::SshConnectionFailed(format!(
            "host key not found for '{}:{}' in {}",
            target.host,
            target.port,
            known_hosts_path.display()
        ))),
        ssh2::CheckResult::Failure => Err(MobileCoreError::SshConnectionFailed(format!(
            "host key verification failed for '{}:{}'",
            target.host, target.port
        ))),
    }
}

fn resolve_known_hosts_path(target: &SshTarget) -> Result<PathBuf> {
    if let Some(path) = target.known_hosts_file.clone() {
        return Ok(path);
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        MobileCoreError::SshConnectionFailed(
            "HOME not set; configure known_hosts_file on SSH target".to_string(),
        )
    })?;
    Ok(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_target_accepts_scheme_and_user() {
        let target = parse_ssh_target("ssh://bmux@prod.example.com:2200")
            .expect("ssh target with scheme should parse");
        assert_eq!(target.user.as_deref(), Some("bmux"));
        assert_eq!(target.host, "prod.example.com");
        assert_eq!(target.port, 2200);
        assert!(target.strict_host_key_checking);
        assert!(target.known_hosts_file.is_none());
        assert!(target.identity_file.is_none());
    }

    #[test]
    fn parse_ssh_target_defaults_port() {
        let target =
            parse_ssh_target("ops@prod.example.com").expect("ssh target should default port");
        assert_eq!(target.user.as_deref(), Some("ops"));
        assert_eq!(target.host, "prod.example.com");
        assert_eq!(target.port, 22);
    }

    #[test]
    fn parse_ssh_target_rejects_bad_port() {
        let error =
            parse_ssh_target("ops@prod.example.com:abc").expect_err("invalid port should fail");
        assert!(matches!(error, MobileCoreError::InvalidTarget(_)));
    }

    #[test]
    fn parse_ssh_target_without_user_defaults_port() {
        let target =
            parse_ssh_target("prod.example.com").expect("ssh target without user should parse");
        assert_eq!(target.user, None);
        assert_eq!(target.host, "prod.example.com");
        assert_eq!(target.port, 22);
    }

    #[test]
    fn parse_ssh_target_query_options() {
        let target = parse_ssh_target(
            "ssh://ops@prod.example.com:2222?strict=false&known_hosts=/tmp/kh&identity=/tmp/id",
        )
        .expect("ssh target with options should parse");

        assert!(!target.strict_host_key_checking);
        assert_eq!(
            target
                .known_hosts_file
                .as_ref()
                .expect("known_hosts should be set")
                .display()
                .to_string(),
            "/tmp/kh"
        );
        assert_eq!(
            target
                .identity_file
                .as_ref()
                .expect("identity should be set")
                .display()
                .to_string(),
            "/tmp/id"
        );
    }
}
