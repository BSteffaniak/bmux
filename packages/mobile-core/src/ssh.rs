use crate::error::{MobileCoreError, Result};

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
}
