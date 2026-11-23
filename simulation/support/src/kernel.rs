//! Questions simulations' checks ask the simulated kernel (`switchy_pty`'s simulator) about the
//! run's processes, terminals and threads, and the descriptions their failures give.

use switchy_pty::simulator::{self, ProcessSnapshot, ProcessState};

/// The processes running now, in the kernel's order.
pub fn running() -> impl Iterator<Item = ProcessSnapshot> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| p.state == ProcessState::Running)
}

/// The process of `serial`, while it runs.
#[must_use]
pub fn running_process(serial: u64) -> Option<ProcessSnapshot> {
    running().find(|p| p.serial == serial)
}

/// The session (its id) of the process of `serial`, running or exited but not yet reaped.
#[must_use]
pub fn session_of(serial: u64) -> Option<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial)
        .map(|p| p.session)
}

/// The session serial of the process of `serial`, while it runs.
#[must_use]
pub fn session_serial_of(serial: u64) -> Option<u64> {
    running_process(serial).map(|p| p.session_serial)
}

/// The processes running in the sessions `serials` name.
#[must_use]
pub fn running_in(serials: &[u64]) -> Vec<u32> {
    running()
        .filter(|p| serials.contains(&p.session_serial))
        .map(|p| p.pid)
        .collect()
}

/// The serials of daemons, which outlive what started them by design.
///
/// By the kernel's facts, not the programs' word (a program killed as it detaches one never
/// records it): started by a process, leading a session of its own, with no terminal.
#[must_use]
pub fn detached() -> Vec<u64> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| {
            let started_by_a_process = p.parent.is_some();
            let without_a_terminal = p.terminal.is_none();
            let leads_its_session = p.session_serial == p.serial;
            started_by_a_process && without_a_terminal && leads_its_session
        })
        .map(|p| p.serial)
        .collect()
}

/// Whether something outside (the kill fault) killed the process of `serial`.
#[must_use]
pub fn killed_by_fault(serial: u64) -> bool {
    simulator::signal_log()
        .iter()
        .any(|record| record.serial == serial && record.by_fault)
}

/// The run's simulated threads that have not finished (but the main thread and daemons), with
/// what each waits for, for a failure's message.
#[must_use]
pub fn live_threads() -> Vec<String> {
    switchy_thread::simulator::scheduler()
        .snapshot()
        .threads
        .into_iter()
        .skip(1)
        .filter(|thread| {
            !thread.daemon && thread.state != switchy_thread::simulator::ThreadState::Finished
        })
        .map(|thread| format!("{} ({:?})", thread.name, thread.state))
        .collect()
}

/// The name of `pid`'s terminal, for events and messages.
#[must_use]
pub fn terminal_of(pid: u32) -> String {
    simulator::process_io(pid)
        .and_then(|io| io.terminal)
        .unwrap_or_else(|| "no terminal".into())
}

/// `pid`'s terminal as the kernel sees it, for a failure's message.
#[must_use]
pub fn terminal_state(pid: u32) -> String {
    let Some(name) = simulator::process_io(pid).and_then(|io| io.terminal) else {
        return "no terminal".into();
    };
    simulator::snapshot()
        .terminals
        .into_iter()
        .find(|t| t.name == name)
        .map_or_else(
            || format!("terminal {name} gone"),
            |t| {
                let written = simulator::terminal_written(&name).unwrap_or_default();
                let tail = &written[written.len().saturating_sub(48)..];
                format!(
                    "terminal {name}: {} bytes unread by the pane, {} typed bytes unread by the \
                     program, {} written, ending \"{}\", masters {}, hung up {}",
                    t.output_buffered,
                    t.input_buffered,
                    t.written,
                    tail.escape_ascii(),
                    t.master_handles,
                    t.hung_up
                )
            },
        )
}
