use super::StopAction;
use portable_pty::MasterPty;
use std::io;

pub(in super::super) fn supports_cooperative_stop() -> bool {
    // Managed Windows executions are owned by the supervisor's Job Object.
    // Its Term operation is explicitly unsupported: proceed to bounded Sweep.
    false
}

pub(in super::super) fn foreground_group(_master: &dyn MasterPty) -> Option<i32> {
    None
}

pub(in super::super) fn stop_foreground_group(
    _master: &dyn MasterPty,
    _action: StopAction,
) -> io::Result<Option<bool>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owned PTY process-group signaling unavailable; managed execution requires a supervisor",
    ))
}

pub(in super::super) fn send_process_signal(
    _process: &bmux_pane_runtime_state::PaneProcessIdentity,
    _signal: bmux_pane_runtime_state::PaneProcessSignal,
) -> anyhow::Result<()> {
    anyhow::bail!("pane process signaling is unsupported on this platform")
}
