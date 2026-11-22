//! Process control on `switchy_pty`'s simulated kernel (the `simulator` feature): the authority
//! the native backend of the run's personality has, over simulated processes.

use crate::platform::StopAction;
use std::io;
use switchy_pty::{
    Signal,
    simulator::{self, ProcessState},
};

/// Windows panes run in a job, which can only be terminated as a whole.
pub(crate) fn supports_cooperative_stop() -> bool {
    !super::windows()
}

/// Signal the terminal's foreground process group; `None` without one, `Some(false)` when it has
/// no running process.
pub(crate) fn stop_foreground_group(
    master: &bmux_pty::PtyMaster,
    action: StopAction,
) -> io::Result<Option<bool>> {
    if super::windows() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY process-group signaling is unavailable on this platform; panes are stopped through their job",
        ));
    }
    let Some(group) = master
        .foreground_process_group()
        .and_then(|group| u32::try_from(group).ok())
        .filter(|group| *group > 0)
    else {
        return Ok(None);
    };
    let running = simulator::snapshot()
        .processes
        .iter()
        .any(|process| process.group == group && process.state == ProcessState::Running);
    if !running {
        return Ok(Some(false));
    }
    let signal = match action {
        StopAction::Cooperative => Signal::Terminate,
        StopAction::Force => Signal::Kill,
    };
    switchy_pty::signal_group(group, signal)?;
    Ok(Some(true))
}

/// Signal a pane's process group, or its process when it has no group.
pub(crate) fn send_process_signal(
    process: &bmux_pane_runtime_state::PaneProcessIdentity,
    signal: bmux_pane_runtime_state::PaneProcessSignal,
) -> anyhow::Result<()> {
    use bmux_pane_runtime_state::PaneProcessSignal;
    if super::windows() {
        anyhow::bail!("pane process signaling is unsupported on this platform");
    }
    let signal = match signal {
        PaneProcessSignal::Interrupt => Signal::Interrupt,
        PaneProcessSignal::Terminate => Signal::Terminate,
        PaneProcessSignal::Kill => Signal::Kill,
        PaneProcessSignal::Hangup => Signal::Hangup,
    };
    if let Some(group) = process
        .process_group_id
        .and_then(|group| u32::try_from(group).ok())
    {
        switchy_pty::signal_group(group, signal)?;
    } else {
        let pid = process
            .pid
            .ok_or_else(|| anyhow::anyhow!("pane process identity is not available"))?;
        simulator::signal_process(pid, signal)?;
    }
    Ok(())
}

pub(crate) fn resolve_process_working_directory(pid: u32) -> Option<String> {
    if super::windows() {
        return None;
    }
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|process| process.pid == pid && process.state == ProcessState::Running)
        .map(|process| process.cwd.to_string_lossy().into_owned())
}
