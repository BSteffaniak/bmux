//! The native Unix target: sessions, process groups and signals.
mod crash_guard;
mod inspection;
mod probe;
mod process;
pub(crate) mod session;
mod session_sweep;

use super::{ProcessStartIdentity, PsProcessEntry};
use std::path::Path;
use std::time::Duration;

pub(crate) use super::native::{path_is_usable_program, program_name_is_on_path};
#[cfg(test)]
pub(crate) use inspection::send_process_signal_command;
pub(crate) use inspection::{resolve_process_working_directory, send_process_signal};
pub(crate) use probe::bounded_probe_output;
pub(crate) use process::{stop_foreground_group, supports_cooperative_stop};

pub(crate) const fn baseline_shell() -> &'static str {
    "/bin/sh"
}

/// Every process's pid, group, state and command, from `ps`.
pub(crate) fn process_table() -> Option<Vec<PsProcessEntry>> {
    let bytes = bounded_probe_output(
        std::process::Command::new("ps").args(["-A", "-o", "pid=,pgid=,state=,command="]),
        Duration::from_millis(200),
    )
    .ok()?;
    Some(
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(parse_ps_process_entry)
            .collect(),
    )
}

pub(crate) fn parse_ps_process_entry(line: &str) -> Option<PsProcessEntry> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let process_group = parts.next()?.parse::<i32>().ok()?;
    let state = parts.next()?.to_string();
    let command = parts.collect::<Vec<_>>().join(" ");

    Some(PsProcessEntry {
        pid,
        pgid: process_group,
        state,
        command,
    })
}

/// Block until `pid` (our child) has exited, without reaping it.
pub(crate) fn wait_for_exit_without_reaping(pid: u32) {
    let pid = libc::id_t::from(pid);
    loop {
        // SAFETY: zeroed siginfo is valid output storage.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waiting on our own child; WNOWAIT leaves it to child.wait().
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            // ECHILD and friends: the caller's own wait reports the outcome.
            return;
        }
    }
}

pub(crate) fn process_start_identity(pid: u32) -> Option<ProcessStartIdentity> {
    #[cfg(target_os = "macos")]
    {
        let pid = i32::try_from(pid).ok()?;
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: the writable buffer has exactly the advertised record size.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: proc_pidinfo returned the complete initialized record.
        let info = unsafe { info.assume_init() };
        Some(ProcessStartIdentity(
            info.pbi_start_tvsec,
            info.pbi_start_tvusec,
        ))
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        let mut stat = String::new();
        std::fs::File::open(format!("/proc/{pid}/stat"))
            .ok()?
            .take(8192)
            .read_to_string(&mut stat)
            .ok()?;
        parse_linux_process_start(&stat)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_linux_process_start(stat: &str) -> Option<ProcessStartIdentity> {
    // comm is parenthesized and may itself contain spaces and parentheses.
    // The suffix begins at field 3; starttime is field 22.
    let (_, suffix) = stat.rsplit_once(')')?;
    let ticks = suffix.split_whitespace().nth(19)?.parse().ok()?;
    Some(ProcessStartIdentity(ticks, 0))
}

pub(crate) fn process_directory(pid: u32) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let pid = i32::try_from(pid).ok()?;
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let size = i32::try_from(std::mem::size_of::<libc::proc_vnodepathinfo>()).ok()?;
        // SAFETY: info points to writable storage of exactly the supplied size.
        // proc_pidinfo initializes this fixed-size record on a complete result.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: the OS returned a complete initialized record above.
        let info = unsafe { info.assume_init() };
        let bytes = info
            .pvi_cdir
            .vip_path
            .into_iter()
            .flatten()
            .map(|byte| byte.to_ne_bytes()[0])
            .take_while(|byte| *byte != 0)
            .collect::<Vec<_>>();
        String::from_utf8(bytes)
            .ok()
            .filter(|path| Path::new(path).is_absolute())
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .and_then(|path| path.into_os_string().into_string().ok())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// Whether a file with `metadata` can be run: any execute bit.
pub(crate) fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

/// Extensions a program name may omit (none on Unix).
pub(crate) const fn executable_extensions() -> Vec<String> {
    Vec::new()
}
