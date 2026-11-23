//! Everything that differs between operating systems, behind one interface.
//!
//! The pane runtime's logic is the same on every operating system; this module is the only place
//! that chooses one. It has two parts:
//!
//! * **The target**: the processes and terminals panes run, with three implementations: native
//!   Unix (`unix`), native Windows (`windows`), and simulated (`simulated`, the `simulator`
//!   feature) on `switchy_pty`'s kernel, which behaves as Linux, macOS or Windows by its
//!   personality. Terminal differences the core logic branches on come from
//!   [`bmux_pty::behavior`].
//! * **The files** ([`files`]): this process's own files and executable: private files and
//!   directories, durable replacement, the executable's identity and retained copies. Natively
//!   these go to the host's file system through `switchy_fs`; simulated (`simulated/files.rs`),
//!   to the run's `switchy_fs` file system, with a stand-in for the executable.
//! * **The host** ([`host`]): this process's native strings and errors, which are the host's
//!   in simulation too, so this part is always native.
//!
//! Core code outside this module names no operating system (`tests/primitives.rs` checks), and
//! reaches files only through `switchy_fs` (so a simulation runs it on the run's file system).

#[cfg(feature = "simulator")]
#[path = "simulated/mod.rs"]
mod target;
#[cfg(all(unix, not(feature = "simulator")))]
#[path = "unix/mod.rs"]
mod target;
#[cfg(all(windows, not(feature = "simulator")))]
#[path = "windows/mod.rs"]
mod target;

#[cfg(not(feature = "simulator"))]
mod native;

#[cfg(any(test, feature = "simulator"))]
pub(crate) mod conformance;

#[cfg(feature = "simulator")]
#[path = "simulated/files.rs"]
pub(crate) mod files;
#[cfg(all(unix, not(feature = "simulator")))]
#[path = "unix/files.rs"]
pub(crate) mod files;
#[cfg(all(windows, not(feature = "simulator")))]
#[path = "windows/files.rs"]
pub(crate) mod files;

#[cfg(unix)]
#[path = "unix/host/mod.rs"]
pub(crate) mod host;
#[cfg(windows)]
#[path = "windows/host/mod.rs"]
pub(crate) mod host;

pub(crate) use target::session;
pub(crate) use target::{
    baseline_shell, path_is_usable_program, process_directory, process_start_identity,
    process_table, program_name_is_on_path, resolve_process_working_directory, send_process_signal,
    stop_foreground_group, supports_cooperative_stop, wait_for_exit_without_reaping,
};
#[cfg(all(unix, test, not(feature = "simulator")))]
pub(crate) use target::{
    bounded_probe_output, parse_ps_process_entry, send_process_signal_command,
};

/// A file as the operating system identifies it (its device, inode, size, mode and change
/// times where it has them): any rewrite or replacement changes it. `None` where it cannot be
/// read, or where a target does not track it (simulated), which callers treat as changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity(pub(crate) [u64; 8]);

/// When a process started, as the operating system records it: with its pid, an identity no
/// later process shares (a reused pid has a different start).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessStartIdentity(pub(crate) u64, pub(crate) u64);

/// One process in the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PsProcessEntry {
    pub(crate) pid: u32,
    pub(crate) pgid: i32,
    /// Its state as `ps` spells it (`Z` for a zombie).
    pub(crate) state: String,
    pub(crate) command: String,
}

/// How hard to stop a terminal's foreground job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StopAction {
    Cooperative,
    Force,
}
