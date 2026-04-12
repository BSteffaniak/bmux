use crate::error::{MobileCoreError, Result};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
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

        session.userauth_agent(&username).map_err(|error| {
            MobileCoreError::SshConnectionFailed(format!(
                "SSH agent auth failed for '{username}@{endpoint}': {error}"
            ))
        })?;
        if !session.authenticated() {
            return Err(MobileCoreError::SshConnectionFailed(format!(
                "SSH auth failed for '{username}@{endpoint}'"
            )));
        }

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

    let (user, host_port) = if let Some((user, rest)) = raw.split_once('@') {
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

    Ok(SshTarget { user, host, port })
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
}
