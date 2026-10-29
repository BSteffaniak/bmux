use super::StopAction;
use portable_pty::MasterPty;
use std::io;

pub(in super::super) fn supports_cooperative_stop() -> bool {
    true
}

pub(in super::super) fn foreground_group(master: &dyn MasterPty) -> Option<i32> {
    master.process_group_leader().filter(|group| *group > 0)
}

/// `None` means the live terminal supplies no foreground-group authority.
pub(in super::super) fn stop_foreground_group(
    master: &dyn MasterPty,
    action: StopAction,
) -> io::Result<Option<bool>> {
    let Some(group) = foreground_group(master) else {
        return Ok(None);
    };
    let signal = match action {
        StopAction::Cooperative => libc::SIGTERM,
        StopAction::Force => libc::SIGKILL,
    };
    // SAFETY: the caller retains the live PTY under its ownership fence through
    // this operation; the group comes from that PTY, never a cached numeric PID.
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(Some(true));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(Some(false));
    }
    Err(error)
}
