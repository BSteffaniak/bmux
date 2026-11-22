//! The native Windows target: Job Objects, pseudo-consoles, and termination instead of signals.
mod process;
pub(crate) mod session;

use super::{ProcessStartIdentity, PsProcessEntry};

pub(crate) use super::native::{path_is_usable_program, program_name_is_on_path};
pub(crate) use process::{send_process_signal, stop_foreground_group, supports_cooperative_stop};

pub(crate) const fn baseline_shell() -> &'static str {
    "cmd.exe"
}

/// Windows has no process groups to inspect.
pub(crate) const fn process_table() -> Option<Vec<PsProcessEntry>> {
    None
}

/// Windows process handles stay valid after exit, so there is no pid to fence.
pub(crate) const fn wait_for_exit_without_reaping(_pid: u32) {}

/// Not observed on Windows.
pub(crate) const fn process_start_identity(_pid: u32) -> Option<ProcessStartIdentity> {
    None
}

/// Not observed on Windows.
pub(crate) const fn process_directory(_pid: u32) -> Option<String> {
    None
}

/// Not observed on Windows.
pub(crate) const fn resolve_process_working_directory(_pid: u32) -> Option<String> {
    None
}

/// Any file can be started; whether it runs is the extension's business.
pub(crate) const fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// Extensions a program name may omit (`PATHEXT`).
pub(crate) fn executable_extensions() -> Vec<String> {
    switchy_env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(|extension| extension.trim().trim_start_matches('.').to_owned())
        .filter(|extension| !extension.is_empty())
        .collect()
}
