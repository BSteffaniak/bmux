//! This process's native strings and errors, on Windows.
pub(crate) mod environment;
mod environment_keys;

/// Windows terminals end with a plain end of file.
pub(crate) const fn is_terminal_end(_error: &std::io::Error) -> bool {
    false
}
