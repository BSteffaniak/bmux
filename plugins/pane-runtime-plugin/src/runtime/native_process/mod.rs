//! Platform-specific authority supplied by a retained, live PTY.
#[cfg(any(windows, test))]
pub(super) mod conpty_startup;
#[cfg(not(unix))]
mod non_unix;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
mod unix_inspection;

#[cfg(not(unix))]
use non_unix as backend;
#[cfg(unix)]
use unix as backend;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopAction {
    Cooperative,
    Force,
}

pub(super) use backend::{foreground_group, stop_foreground_group, supports_cooperative_stop};

#[cfg(not(unix))]
pub(super) use non_unix::send_process_signal;
#[cfg(all(unix, test))]
pub(super) use unix_inspection::send_process_signal_command;
#[cfg(unix)]
pub(super) use unix_inspection::{
    process_command_looks_like_shell, resolve_process_working_directory, send_process_signal,
};
