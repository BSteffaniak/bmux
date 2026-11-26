use thiserror::Error;

pub type Result<T> = std::result::Result<T, MobileCoreError>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MobileCoreError {
    #[error("invalid pairing link: {0}")]
    InvalidPairingLink(String),
    #[error("pairing {0} not found")]
    PairingNotFound(String),
    #[error("no saved host named '{0}'")]
    HostNotFound(String),
    #[error("saved hosts: {0}")]
    HostStore(String),
    #[error("{0}")]
    ConnectionFailed(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("connection {0} is not active")]
    ConnectionNotActive(String),
    #[error("terminal session {0} not found")]
    TerminalSessionNotFound(String),
    #[error("terminal session {0} is closed")]
    TerminalSessionClosed(String),
    #[error("invalid terminal size rows={rows}, cols={cols}")]
    InvalidTerminalSize { rows: u16, cols: u16 },
    #[error("terminal backend failure: {0}")]
    TerminalBackendFailure(String),
}
