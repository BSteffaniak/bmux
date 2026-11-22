//! This process's native strings and errors, on Unix.
pub(crate) mod environment;

/// Whether a terminal read failed only because the terminal ended (`EIO` once the last slave
/// closes).
pub(crate) fn is_terminal_end(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::EIO)
}
