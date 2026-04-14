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
    #[error("terminal session {0} not found")]
    TerminalSessionNotFound(String),
    #[error("terminal session {0} is closed")]
    TerminalSessionClosed(String),
    #[error("invalid terminal size rows={rows}, cols={cols}")]
    InvalidTerminalSize { rows: u16, cols: u16 },
    #[error("unsupported terminal transport target: {0}")]
    UnsupportedTerminalTransport(String),
    #[error("terminal backend failure: {0}")]
    TerminalBackendFailure(String),
}
