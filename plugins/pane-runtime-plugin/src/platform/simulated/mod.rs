//! The simulated target: `switchy_pty`'s kernel, behaving as Linux, macOS or Windows by the
//! run's personality. What a native target does on one operating system, this one does for the
//! personality's: Windows' Job Objects and terminations, or Unix sessions, groups and signals.
mod process;
pub(crate) mod session;

use super::{ProcessStartIdentity, PsProcessEntry};
use std::path::Path;
use std::time::Duration;
use switchy_pty::simulator::{self, ProcessState};

pub(crate) use process::{
    resolve_process_working_directory, send_process_signal, stop_foreground_group,
    supports_cooperative_stop,
};

/// Whether the run's kernel behaves as Windows.
pub(crate) fn windows() -> bool {
    simulator::personality().is_windows()
}

pub(crate) fn baseline_shell() -> &'static str {
    if windows() { "cmd.exe" } else { "/bin/sh" }
}

/// A program is usable when the kernel has one by that name.
pub(crate) fn path_is_usable_program(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(simulator::program_registered)
}

pub(crate) fn program_name_is_on_path(program: &str) -> bool {
    simulator::program_registered(program)
}

/// The kernel's processes, as `ps` would list them (Windows has no groups to list).
pub(crate) fn process_table() -> Option<Vec<PsProcessEntry>> {
    if windows() {
        return None;
    }
    Some(
        simulator::snapshot()
            .processes
            .into_iter()
            .filter_map(|process| {
                Some(PsProcessEntry {
                    pid: process.pid,
                    pgid: i32::try_from(process.group).ok()?,
                    state: if process.state == ProcessState::Running {
                        "S".to_owned()
                    } else {
                        "Z".to_owned()
                    },
                    command: process.program,
                })
            })
            .collect(),
    )
}

/// Block until `pid` (our child) has exited, without reaping it.
pub(crate) fn wait_for_exit_without_reaping(pid: u32) {
    // Polled on the run's clock: the simulated kernel has no waitid.
    while switchy_pty::simulator::snapshot()
        .processes
        .iter()
        .any(|process| process.pid == pid && process.state == ProcessState::Running)
    {
        switchy_thread::thread::sleep(Duration::from_millis(1));
    }
}

/// A simulated process's serial is unique for the whole run, as a start time is.
pub(crate) fn process_start_identity(pid: u32) -> Option<ProcessStartIdentity> {
    if windows() {
        return None;
    }
    simulator::serial_of(pid).map(|serial| ProcessStartIdentity(serial, 0))
}

pub(crate) fn process_directory(pid: u32) -> Option<String> {
    if windows() {
        return None;
    }
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|process| process.pid == pid)
        .map(|process| process.cwd.to_string_lossy().into_owned())
}
