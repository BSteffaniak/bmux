//! What the simulated kernel says about the run's processes, terminals and threads, for the
//! workload's checks and its failures' messages.

use switchy_pty::simulator::{self, ProcessState};

/// The process of `serial`, while it runs: its id, session and terminal.
pub(super) fn alive(serial: u64) -> Option<(u32, u32, Option<String>)> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial && p.state == ProcessState::Running)
        .map(|p| (p.pid, p.session, p.terminal))
}

/// The session of the process of `serial`, running or exited but not yet reaped.
pub(super) fn leader_of(serial: u64) -> Option<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial)
        .map(|p| p.session)
}

/// The session serial of the process of `serial`, while it runs.
pub(super) fn session_of(serial: u64) -> Option<u64> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|p| p.serial == serial && p.state == ProcessState::Running)
        .map(|p| p.session_serial)
}

/// The serials of daemons panes' programs detached (which outlive their panes by design): by
/// the kernel's facts, not the programs' word (a program killed as it detaches one never
/// records it). Started by a process, leading a session of its own, with no terminal.
pub(super) fn detached() -> Vec<u64> {
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
pub(super) fn killed_by_fault(serial: u64) -> bool {
    simulator::signal_log()
        .iter()
        .any(|record| record.serial == serial && record.by_fault)
}

/// The processes running in the sessions `serials` name.
pub(super) fn running_in(serials: &[u64]) -> Vec<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|p| p.state == ProcessState::Running && serials.contains(&p.session_serial))
        .map(|p| p.pid)
        .collect()
}

/// The run's simulated threads that have not finished (but the main thread), with what each
/// waits for, for a failure's message.
pub(super) fn live_threads() -> Vec<String> {
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

/// The name of `pid`'s terminal, for events.
pub(super) fn terminal_of(pid: u32) -> String {
    simulator::process_io(pid)
        .and_then(|io| io.terminal)
        .unwrap_or_else(|| "no terminal".into())
}

/// `pid`'s terminal as the kernel sees it, for a failure's message.
pub(super) fn terminal_state(pid: u32) -> String {
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

/// Whether `pid`'s terminal shows each of `lines` answered, in order.
pub(super) fn answered(pid: u32, lines: &[String]) -> bool {
    let Some(terminal) = simulator::process_io(pid).and_then(|io| io.terminal) else {
        return lines.is_empty();
    };
    // An interrupt typed while a program reads raw input (a querier waiting for its answers)
    // is an ordinary byte, so it joins the next line.
    let written =
        String::from_utf8_lossy(&simulator::terminal_written(&terminal).unwrap_or_default())
            .replace('\x03', "");
    let mut rest = written.as_str();
    lines.iter().all(|line| {
        let answer = format!("got:{line}\r\n");
        rest.find(&answer).is_some_and(|at| {
            rest = &rest[at + answer.len()..];
            true
        })
    })
}
