use thiserror::Error;

pub type Result<T> = std::result::Result<T, MobileCoreError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MobileCoreError {
    #[error("invalid target: {0}")]
    InvalidTarget(String),
    #[error("target id not found: {0}")]
    TargetNotFound(String),
    #[error("connection {0} is not active")]
    ConnectionNotActive(String),
    #[error("ssh backend is not configured")]
    SshBackendUnavailable,
    #[error("ssh connection failed: {0}")]
    SshConnectionFailed(String),
}
