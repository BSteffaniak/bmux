use crate::platform::StopAction;
use std::io;

pub(crate) fn supports_cooperative_stop() -> bool {
    // Windows panes run in a Job Object, which can only be terminated as a
    // whole: callers escalate straight to a forced sweep of the job.
    false
}

pub(crate) fn stop_foreground_group(
    _master: &bmux_pty::PtyMaster,
    _action: StopAction,
) -> io::Result<Option<bool>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "PTY process-group signaling is unavailable on this platform; panes are stopped through their job",
    ))
}

pub(crate) fn send_process_signal(
    _process: &bmux_pane_runtime_state::PaneProcessIdentity,
    _signal: bmux_pane_runtime_state::PaneProcessSignal,
) -> anyhow::Result<()> {
    anyhow::bail!("pane process signaling is unsupported on this platform")
}
