//! Platform-specific authority supplied by a retained, live PTY.
#[cfg(any(windows, test))]
pub(super) mod conpty_startup;
#[cfg(not(any(unix, feature = "simulator")))]
mod non_unix;
#[cfg(feature = "simulator")]
mod simulated;
#[cfg(all(unix, not(feature = "simulator")))]
mod unix;
#[cfg(unix)]
mod unix_inspection;

#[cfg(not(any(unix, feature = "simulator")))]
use non_unix as backend;
#[cfg(feature = "simulator")]
use simulated as backend;
#[cfg(all(unix, not(feature = "simulator")))]
use unix as backend;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopAction {
    Cooperative,
    Force,
}

pub(super) use backend::{stop_foreground_group, supports_cooperative_stop};

#[cfg(not(any(unix, feature = "simulator")))]
pub(super) use non_unix::send_process_signal;
#[cfg(feature = "simulator")]
pub(super) use simulated::{resolve_process_working_directory, send_process_signal};
#[cfg(unix)]
pub(super) use unix_inspection::process_command_looks_like_shell;
#[cfg(all(unix, test))]
pub(super) use unix_inspection::send_process_signal_command;
#[cfg(all(unix, not(feature = "simulator")))]
pub(super) use unix_inspection::{resolve_process_working_directory, send_process_signal};
