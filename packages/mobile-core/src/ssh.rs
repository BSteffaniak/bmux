use crate::error::{MobileCoreError, Result};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
    pub host_key_fingerprint_sha256: Option<String>,
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

    /// Fetch the server's observed host-key SHA-256 fingerprint.
    ///
    /// # Errors
    ///
    /// Returns connection or handshake errors, or an error when the server
    /// does not provide a SHA-256 host key hash.
    fn observe_host_key_fingerprint_sha256(&self, target: &SshTarget) -> Result<String>;
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
        let session = self.handshaked_session(target)?;

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

    fn observe_host_key_fingerprint_sha256(&self, target: &SshTarget) -> Result<String> {
        let session = self.handshaked_session(target)?;
        let hash = session
            .host_key_hash(ssh2::HashType::Sha256)
            .ok_or_else(|| {
                MobileCoreError::SshConnectionFailed(format!(
                    "SSH server did not provide SHA-256 host key hash for '{}:{}'",
                    target.host, target.port
                ))
            })?;
        Ok(to_hex(hash))
    }
}

impl EmbeddedSshBackend {
    fn handshaked_session(&self, target: &SshTarget) -> Result<ssh2::Session> {
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
        Ok(session)
    }
}

#[derive(Debug, Default)]
pub struct MockSshBackend;

impl SshBackend for MockSshBackend {
    fn open(&self, _target: &SshTarget) -> Result<()> {
        Ok(())
    }

    fn observe_host_key_fingerprint_sha256(&self, _target: &SshTarget) -> Result<String> {
        Ok("0000000000000000000000000000000000000000000000000000000000000000".to_string())
    }
}

/// Resolve a target string and fetch the observed SSH host-key SHA-256
/// fingerprint from the server.
///
/// # Errors
///
/// Returns target parse, DNS/network, handshake, or fingerprint availability
/// errors.
pub fn observe_ssh_host_key_fingerprint_sha256(target: &str) -> Result<String> {
    let parsed = parse_ssh_target(target)?;
    EmbeddedSshBackend::default().observe_host_key_fingerprint_sha256(&parsed)
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
        (None, host_input)
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
    let mut host_key_fingerprint_sha256 = None;
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
                "host_key_fp" | "host_key_fingerprint" | "host_key_sha256" => {
                    if !value.is_empty() {
                        host_key_fingerprint_sha256 = Some(normalize_sha256_fingerprint(value)?);
                    }
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
        host_key_fingerprint_sha256,
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

fn normalize_sha256_fingerprint(value: &str) -> Result<String> {
    let trimmed = value.trim();
    let candidate = value
        .trim()
        .strip_prefix("sha256:")
        .unwrap_or(trimmed)
        .replace(':', "")
        .to_ascii_lowercase();

    let is_hex = candidate.chars().all(|ch| ch.is_ascii_hexdigit());
    if !is_hex || candidate.len() != 64 {
        return Err(MobileCoreError::InvalidTarget(format!(
            "invalid SHA-256 host key fingerprint '{value}'"
        )));
    }
    Ok(candidate)
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
    if let Some(expected_fingerprint) = target.host_key_fingerprint_sha256.as_ref() {
        verify_pinned_fingerprint(session, expected_fingerprint)?;
    }

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

fn verify_pinned_fingerprint(session: &ssh2::Session, expected_fingerprint: &str) -> Result<()> {
    let raw_hash = session
        .host_key_hash(ssh2::HashType::Sha256)
        .ok_or_else(|| {
            MobileCoreError::SshConnectionFailed(
                "SSH server did not provide SHA-256 host key hash".to_string(),
            )
        })?;
    let actual_fingerprint = to_hex(raw_hash);

    if actual_fingerprint == expected_fingerprint {
        return Ok(());
    }

    Err(MobileCoreError::SshConnectionFailed(format!(
        "host key fingerprint mismatch (expected {expected_fingerprint}, got {actual_fingerprint})"
    )))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(hex_char(byte >> 4));
        output.push(hex_char(byte & 0x0F));
    }
    output
}

const fn hex_char(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'a' + (value - 10)) as char,
        _ => '0',
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
        assert!(target.host_key_fingerprint_sha256.is_none());
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
            "ssh://ops@prod.example.com:2222?strict=false&host_key_fp=sha256:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB:AB&known_hosts=/tmp/kh&identity=/tmp/id",
        )
        .expect("ssh target with options should parse");

        assert!(!target.strict_host_key_checking);
        assert_eq!(
            target
                .host_key_fingerprint_sha256
                .as_deref()
                .expect("fingerprint should be set"),
            "abababababababababababababababababababababababababababababababab"
        );
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

    #[test]
    fn parse_ssh_target_rejects_invalid_host_key_fingerprint() {
        let error = parse_ssh_target("ssh://ops@prod.example.com?host_key_fp=xyz")
            .expect_err("invalid fingerprint should fail");
        assert!(matches!(error, MobileCoreError::InvalidTarget(_)));
    }

    #[test]
    fn mock_backend_reports_fingerprint() {
        let backend = MockSshBackend;
        let target =
            parse_ssh_target("ssh://ops@prod.example.com:22").expect("target should parse");
        let fingerprint = backend
            .observe_host_key_fingerprint_sha256(&target)
            .expect("mock backend should provide fingerprint");
        assert_eq!(fingerprint.len(), 64);
    }
}
