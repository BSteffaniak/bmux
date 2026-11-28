//! The actions on panes: opening, typing, resizing, interrupting, splitting, restarting and
//! closing them, and the invariants each checks.

use std::sync::{Arc, atomic::Ordering}; // simulation-safe: the run's own server generation

use bmux_pane_runtime_plugin::simulation::{Pane, Panes, Restarted};
use bmux_simvar_tui::Tone;
use sim_support::{
    invariants::checked,
    kernel::{
        killed_by_fault, running_in, running_process, session_of, session_serial_of, terminal_of,
        terminal_state,
    },
};
use switchy_pty::simulator;

use super::{Opening, Shared, draw, event, ledger::Opened, random_size};
use crate::{
    invariants::Invariant,
    telemetry::{Board, PaneView},
};

// Each locked section is a deliberate unit (see the workload's other `impl Shared`).
#[allow(clippy::significant_drop_tightening)]
impl Shared {
    /// What is wrong, if anything, with `pane`'s program (`pid`, `serial`) leading its own
    /// session: it must, unless it already ended and the runtime knows (or the kill fault
    /// killed it).
    async fn leads_own_session(
        &self,
        panes: &Panes,
        pane: Pane,
        pid: u32,
        serial: u64,
    ) -> Option<String> {
        match session_of(serial) {
            Some(session) if session == pid => None,
            Some(session) => Some(format!("it is in session {session}")),
            None if killed_by_fault(serial) => None,
            // Reaped: the runtime notices a moment later.
            None if self
                .settle(|| panes.exited(pane) || panes.pid(pane) != Some(pid))
                .await =>
            {
                None
            }
            None => Some("it is gone and the pane never noticed".into()),
        }
    }

    /// Whether a fault the profile has on may make opening a pane fail, or a program it may
    /// run exits before its pane is ready (the runtime then refuses the pane).
    fn opens_may_fail(&self) -> bool {
        let faults = &self.profile.faults;
        faults.spawn > 0.0
            || faults.thread_spawn > 0.0
            || faults.kill > 0.0
            || faults.read_error > 0.0
            || faults.file > 0
            || self.profile.program_mix().iter().any(|(kind, _)| {
                matches!(
                    kind,
                    crate::programs::Kind::Instant | crate::programs::Kind::Orphaner
                )
            })
    }

    /// Open a pane, checking invariant 1.
    pub(super) async fn open(&self, driver: u64) {
        let (panes, generation) = {
            let mut state = self.lock();
            if state.open.len() + state.opening >= self.profile.panes {
                return;
            }
            state.opening += 1;
            (
                Arc::clone(&state.panes),
                self.generation.load(Ordering::SeqCst),
            )
        };
        let _opening = Opening(self, generation);
        let (rows, cols) = random_size();
        let pane = match panes.open(rows, cols).await {
            Ok(pane) => pane,
            Err(error) => {
                self.lock().stats.failed_opens += 1;
                if !self.opens_may_fail() {
                    self.violated(
                        Invariant::SessionLeader,
                        format!("opening a pane failed: {error:#}"),
                    );
                }
                return;
            }
        };
        let Some(pid) = panes.pid(pane) else {
            self.violated(Invariant::SessionLeader, "an opened pane has no process");
            return;
        };
        // By the runtime's own record: the id may already be another process's.
        let serial = panes
            .program_serial(pane)
            .or_else(|| simulator::serial_of(pid))
            .unwrap_or(0);
        // 1. Its program leads its own session (and if it already ended, the runtime knows).
        checked(Invariant::SessionLeader);
        if let Some(problem) = self.leads_own_session(&panes, pane, pid, serial).await {
            self.violated(
                Invariant::SessionLeader,
                format!("pane process {pid} does not lead its own session: {problem}"),
            );
        }
        event(
            "open",
            format!(
                "d{driver}: pane {pid} opened at {rows}x{cols} on {}",
                terminal_of(pid)
            ),
            Tone::Info,
        );
        Board::pane(PaneView {
            pane: format!("{pane:?}"),
            pid,
            size: (rows, cols),
            typed: 0,
            open: true,
        });
        let mut state = self.lock();
        state.stats.opened += 1;
        state.order.push(pane);
        state.open.insert(
            pane,
            Opened {
                pid,
                serial,
                terminal: terminal_by_serial(serial),
                typed: Vec::new(),
                programs: 1,
                restarting: false,
                resizes: 0,
                touched: 0,
            },
        );
    }

    /// Type a random line into a pane (sometimes `bg`, starting a job that ignores hang-ups).
    /// Atomic with respect to other drivers: the pane cannot change between choosing it and
    /// recording the line.
    pub(super) fn type_line(&self, driver: u64) {
        let mut state = self.lock();
        let Some(pane) = Self::pick(&state, driver) else {
            return;
        };
        let scripted = state.targets.get(&driver).and_then(|step| step.line);
        let line: String = scripted.map_or_else(
            || {
                if draw(5) == 0 {
                    "bg".to_owned()
                } else if draw(20) == 0 {
                    // The shell ends: the pane stays, its program exited.
                    "exit".to_owned()
                } else {
                    (0..=draw(20))
                        .map(|_| char::from(b'a' + u8::try_from(draw(26)).unwrap_or(0)))
                        .collect()
                }
            },
            str::to_owned,
        );
        if !state.panes.input(pane, format!("{line}\r").as_bytes()) {
            return;
        }
        state.stats.typed += 1;
        let Some(opened) = state.open.get_mut(&pane) else {
            return;
        };
        event(
            "type",
            format!("d{driver}: typed {line:?} into {}", opened.pid),
            Tone::Neutral,
        );
        // Mid-restart (only scripts type then), the old program or the new one may read it.
        if opened.restarting {
            return;
        }
        opened.typed.push(line);
        Board::typed(&format!("{pane:?}"));
    }

    /// Split a pane: its session gains a pane running a new program, checking invariant 1.
    pub(super) async fn split(&self, driver: u64) {
        let (pane, panes) = {
            let mut state = self.lock();
            if state.open.len() + state.opening >= self.profile.panes {
                return;
            }
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            state.opening += 1;
            (pane, Arc::clone(&state.panes))
        };
        let _opening = Opening(self, self.generation.load(Ordering::SeqCst));
        let (new, pid) = match panes.split(pane, draw(2) == 0).await {
            Ok((new, Restarted::Running(pid))) => (new, pid),
            Ok((_, Restarted::Gone)) => return,
            Ok((_, Restarted::Failed)) | Err(_) => {
                self.lock().stats.failed_opens += 1;
                // A split of a pane closed meanwhile, or a program failing to start, is refused.
                return;
            }
        };
        let serial = panes
            .program_serial(new)
            .or_else(|| simulator::serial_of(pid))
            .unwrap_or(0);
        checked(Invariant::SessionLeader);
        if let Some(problem) = self.leads_own_session(&panes, new, pid, serial).await {
            self.violated(
                Invariant::SessionLeader,
                format!("split pane process {pid} does not lead its own session: {problem}"),
            );
        }
        event(
            "split",
            format!("d{driver}: pane {pid} split off on {}", terminal_of(pid)),
            Tone::Info,
        );
        let mut state = self.lock();
        state.stats.opened += 1;
        state.order.push(new);
        state.open.insert(
            new,
            Opened {
                pid,
                serial,
                terminal: terminal_by_serial(serial),
                typed: Vec::new(),
                programs: 1,
                restarting: false,
                resizes: 0,
                touched: 0,
            },
        );
    }

    /// Interrupt a pane's program (`ctrl+c`): its foreground program usually ends.
    pub(super) fn interrupt(&self, driver: u64) {
        let mut state = self.lock();
        let Some(pane) = Self::pick(&state, driver) else {
            return;
        };
        if state.panes.input(pane, b"\x03")
            && let Some(opened) = state.open.get_mut(&pane)
        {
            opened.touched += 1;
            event(
                "interrupt",
                format!("d{driver}: interrupted {}", opened.pid),
                Tone::Neutral,
            );
        }
    }

    /// Observe a pane as a client does, checking invariant 15 as its frames apply, or (one
    /// time in four) stop observing it, as a client detaching does.
    pub(super) fn observe(&self, driver: u64) {
        let (pane, panes) = {
            let state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            (pane, Arc::clone(&state.panes))
        };
        if draw(4) == 0 {
            panes.unobserve(pane, OBSERVER);
            return;
        }
        checked(Invariant::ReplicaLevel);
        if let Err(problem) = panes.observe(pane, OBSERVER) {
            self.violated(
                Invariant::ReplicaLevel,
                format!("observing a pane: {problem}"),
            );
        }
    }

    /// Resize a pane, checking invariant 3.
    /// Attach to a pane's session as a client does and take an attach snapshot of the pane
    /// (again, when attached: as a client does on a layout change), or, one time in four,
    /// detach. Checks invariant 16.
    pub(super) fn attach(&self, driver: u64) {
        let (pane, panes) = {
            let state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            (pane, Arc::clone(&state.panes))
        };
        if draw(4) == 0 {
            panes.detach(pane, ATTACHER);
            return;
        }
        // 16. An attach snapshot shows a state the pane's grid passed through.
        checked(Invariant::AttachSnapshot);
        if let Err(problem) = panes.attach(pane, ATTACHER) {
            self.violated(
                Invariant::AttachSnapshot,
                format!("attaching to a pane: {problem}"),
            );
        }
    }

    pub(super) async fn resize(&self, driver: u64) {
        let (pane, rows, cols, epoch, panes) = {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            let (rows, cols) = random_size();
            if !state.panes.resize(pane, rows, cols) {
                return;
            }
            let panes = Arc::clone(&state.panes);
            state.stats.resized += 1;
            let Some(opened) = state.open.get_mut(&pane) else {
                return;
            };
            opened.resizes += 1;
            event(
                "resize",
                format!("d{driver}: pane {} resized to {rows}x{cols}", opened.pid),
                Tone::Neutral,
            );
            (pane, rows, cols, opened.resizes, panes)
        };
        Board::resized(&format!("{pane:?}"), (rows, cols));
        // 3. The terminal ends up the requested size, unless something superseded the resize:
        // a later one, a restart, a close, or the program ending.
        checked(Invariant::RequestedSize);
        let current = || {
            let state = self.lock();
            state
                .open
                .get(&pane)
                .filter(|opened| opened.resizes == epoch && !opened.restarting)
                .map(|opened| opened.serial)
        };
        let sized = self
            .settle(|| {
                let Some(serial) = current() else {
                    return true;
                };
                let Some(terminal) = running_process(serial).and_then(|p| p.terminal) else {
                    return true;
                };
                simulator::snapshot()
                    .terminals
                    .iter()
                    .any(|t| t.name == terminal && (t.size.rows, t.size.cols) == (rows, cols))
            })
            .await;
        if !sized && current().is_some_and(|serial| running_process(serial).is_some()) {
            let pid = panes.pid(pane).unwrap_or(0);
            self.violated(
                Invariant::RequestedSize,
                format!("pane process {pid}'s terminal never became {rows}x{cols}"),
            );
        }
    }

    /// Close a pane, checking invariants 2 and 4.
    pub(super) async fn close_one(&self, driver: u64) {
        let (pane, opened, panes) = {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            let Some(opened) = state.open.remove(&pane) else {
                return;
            };
            state.stats.closed += 1;
            (pane, opened, Arc::clone(&state.panes))
        };
        event(
            "close",
            format!("d{driver}: pane {} closing", opened.pid),
            Tone::Neutral,
        );
        self.close(&panes, pane, &opened).await;
        Board::closed(&format!("{pane:?}"));
    }

    /// Invariants 13, 14 and 15, as `pane` (whose program was `opened`) closes: its screen,
    /// what it took in, and an observing client's replica of it.
    fn check_screen(&self, panes: &Panes, pane: Pane, opened: &Opened) {
        let pid = opened.pid;
        // 14. A pane takes in what its terminal gave it (its program's output and the terminal's
        // echoes), in order.
        if opened.programs == 1
            && let (Some(terminal), Some(chunks)) = (&opened.terminal, panes.taken_in(pane))
        {
            checked(Invariant::OutputDelivered);
            let given = simulator::terminal_output(terminal).unwrap_or_default();
            // A console's own cursor query, which it opens with, the runtime answers as the
            // pane starts and never shows.
            let given = if switchy_pty::behavior().startup_cursor_query {
                given.strip_prefix(b"\x1b[6n").unwrap_or(&given)
            } else {
                &given
            };
            if let Some(problem) = delivery_problem(given, &chunks) {
                self.violated(
                    Invariant::OutputDelivered,
                    format!("pane process {pid} ({terminal}): {problem}"),
                );
            }
        }
        // 15. A client's replica of a pane shows what the pane's grid shows.
        if let Some(level) = panes.observed_level(pane, OBSERVER) {
            checked(Invariant::ReplicaLevel);
            if let Err(difference) = level {
                self.violated(
                    Invariant::ReplicaLevel,
                    format!("pane process {pid}: {difference}"),
                );
            }
        }
        // 13. A pane's screen is what its input made it.
        checked(Invariant::ScreenFromInput);
        if let Some(Err(difference)) = panes.screen_check(pane) {
            self.violated(
                Invariant::ScreenFromInput,
                format!("pane process {pid}: {difference}"),
            );
        }
    }

    /// Close `pane` (already out of the open set), checking invariants 2 and 4.
    pub(super) async fn close(&self, panes: &Panes, pane: Pane, opened: &Opened) {
        // 2. Every typed line reached the program, in order (unless it ended meanwhile).
        checked(Invariant::TypedInOrder);
        let pid = opened.pid;
        let reached = self
            .settle(|| answered(pid, &opened.typed) || running_process(opened.serial).is_none())
            .await;
        if !reached {
            self.violated(
                Invariant::TypedInOrder,
                format!(
                    "pane process {pid} ({}) never answered all of {:?} ({})",
                    crate::programs::kind_of(opened.serial)
                        .map_or("unknown program", crate::programs::Kind::name),
                    opened.typed,
                    terminal_state(pid)
                ),
            );
        }
        // 10. A pane whose program ended says so.
        if running_process(opened.serial).is_none() {
            checked(Invariant::EndedSaysSo);
            if !self
                .settle(|| panes.exited(pane) || panes.pid(pane) != Some(pid))
                .await
            {
                self.violated(
                    Invariant::EndedSaysSo,
                    format!(
                        "pane process {pid} ({}) ended but its pane never said so",
                        crate::programs::kind_of(opened.serial)
                            .map_or("unknown program", crate::programs::Kind::name)
                    ),
                );
            }
        }
        self.check_screen(panes, pane, opened);
        // Its program leads its session (invariant 1), whose serial is the program's: it may
        // have exited (and been reaped), leaving jobs behind in it.
        let session = session_serial_of(opened.serial)
            .or_else(|| (opened.serial != 0).then_some(opened.serial));
        // 11. Closing a pane leaves its siblings (its session's other panes) alone.
        let siblings = self.siblings_of(panes, pane);
        let signalled_before: usize = simulator::signal_log().len();
        match panes.close(pane).await {
            Ok(closed) => {
                // 4. Closing drains, cleans up, and leaves nothing running in the session.
                checked(Invariant::CloseCleansUp);
                // (A failing disk may keep a session's record from being deleted.)
                if let Some(error) = closed
                    .cleanup_error
                    .as_ref()
                    .filter(|_| self.profile.faults.file == 0)
                {
                    self.violated(
                        Invariant::CloseCleansUp,
                        format!("closing pane process {pid} failed to clean up: {error}"),
                    );
                }
                // (Unless the runtime released them: then they outlive the pane by design.)
                if let Some(session) =
                    session.filter(|session| !panes.released_sessions().contains(session))
                {
                    let ended = self.settle(|| running_in(&[session]).is_empty()).await;
                    if !ended {
                        self.violated(
                            Invariant::CloseCleansUp,
                            format!(
                                "pane process {pid}'s session still runs {:?} after it closed",
                                running_in(&[session])
                            ),
                        );
                    }
                }
            }
            Err(error) => self.violated(
                Invariant::CloseCleansUp,
                format!("closing a pane failed: {error:#}"),
            ),
        }
        self.check_siblings(pid, session, siblings, signalled_before)
            .await;
    }

    /// The panes `pane`'s session has besides it, with their programs (id, serial) and how
    /// often they were touched, for invariant 11.
    fn siblings_of(&self, panes: &Panes, pane: Pane) -> Vec<(Pane, u32, u64, u64)> {
        let state = self.lock();
        panes
            .siblings(pane)
            .into_iter()
            .filter(|other| *other != pane)
            .filter_map(|other| {
                state
                    .open
                    .get(&other)
                    .map(|o| (other, o.pid, o.serial, o.touched))
            })
            .collect()
    }

    /// Invariant 11: the close of pane process `pid` (whose session was `session`; signals from
    /// index `signalled_before` of the log on) sent nothing that ends a sibling.
    async fn check_siblings(
        &self,
        pid: u32,
        session: Option<u64>,
        siblings: Vec<(Pane, u32, u64, u64)>,
        signalled_before: usize,
    ) {
        // Only when attribution is exact: no other driver acts meanwhile, and the siblings run
        // shells that end only when told to (so any signal that ends one is the close's).
        // (Scripts also attribute exactly: what another driver does to a sibling touches it.)
        let scripted = self.lock().scripts.is_some();
        let siblings: Vec<_> = if self.profile.drivers == 1 || scripted {
            let state = self.lock();
            siblings
                .into_iter()
                .filter(|(other, _, serial, _)| {
                    matches!(
                        crate::programs::kind_of(*serial),
                        Some(crate::programs::Kind::Shell | crate::programs::Kind::Stubborn)
                    ) && state
                        .open
                        .get(other)
                        .is_some_and(|o| !o.typed.iter().any(|line| line == "exit"))
                })
                .collect()
        } else {
            Vec::new()
        };
        if !siblings.is_empty() {
            checked(Invariant::SiblingsAlone);
            // Let the close finish (another pane's closes in the background).
            let _ = self
                .settle(|| session.is_none_or(|s| running_in(&[s]).is_empty()))
                .await;
            // Only what would end them counts, and only for siblings nothing else touched
            // meanwhile (another driver may interrupt, restart or close them).
            // (A restart under way when the close began ends the sibling's old program: it then
            // runs another.)
            let untouched = |other: Pane, serial: u64, touched: u64| {
                self.lock()
                    .open
                    .get(&other)
                    .is_some_and(|o| o.touched == touched && !o.restarting && o.serial == serial)
            };
            for record in simulator::signal_log().iter().skip(signalled_before) {
                if record.by_fault
                    || !matches!(
                        record.signal,
                        switchy_pty::Signal::Kill
                            | switchy_pty::Signal::Hangup
                            | switchy_pty::Signal::Terminate
                    )
                {
                    continue;
                }
                if let Some((_, sibling, _, _)) =
                    siblings.iter().find(|(other, _, serial, touched)| {
                        *serial == record.serial && untouched(*other, *serial, *touched)
                    })
                {
                    self.violated(Invariant::SiblingsAlone, format!(
                        "closing pane process {pid} sent {:?} to its sibling pane's process {sibling}",
                        record.signal
                    ));
                }
            }
        }
    }

    /// Restart a pane, checking invariant 7.
    pub(super) async fn restart(&self, driver: u64) {
        let begun = 'begin: {
            let mut state = self.lock();
            let Some(pane) = Self::pick(&state, driver) else {
                return;
            };
            let panes = Arc::clone(&state.panes);
            let Some(opened) = state.open.get_mut(&pane) else {
                break 'begin Err((pane, panes));
            };
            let Some(old_session) = session_serial_of(opened.serial) else {
                return;
            };
            // Lines typed from now on go to the new program.
            opened.restarting = true;
            opened.touched += 1;
            opened.programs += 1;
            opened.typed.clear();
            Ok((pane, opened.pid, old_session, panes))
        };
        let (pane, old, old_session, panes) = match begun {
            Ok(begun) => begun,
            Err((pane, panes)) => {
                // A scripted restart of a pane closed (or closing) meanwhile: the runtime may
                // refuse it or restart it, but nothing it starts may outlive the pane
                // (invariant 9, at the end).
                let restarted = panes.restart(pane).await;
                event(
                    "restart",
                    format!("d{driver}: restarting a closed pane: {restarted:?}"),
                    Tone::Neutral,
                );
                return;
            }
        };
        let restarted = panes.restart(pane).await;
        let pid = match restarted {
            Ok(Restarted::Running(pid)) => pid,
            Ok(Restarted::Gone) => return,
            // A scripted close of the pane meanwhile ends the restart too.
            Ok(Restarted::Failed) | Err(_) if !self.lock().open.contains_key(&pane) => return,
            Ok(Restarted::Failed) => {
                if !self.opens_may_fail() {
                    self.violated(
                        Invariant::RestartEndsSession,
                        format!("restarting pane process {old}: its new program failed to start"),
                    );
                }
                if let Some(opened) = self.lock().open.get_mut(&pane) {
                    opened.restarting = false;
                }
                return;
            }
            Err(error) => {
                if !self.opens_may_fail() {
                    self.violated(
                        Invariant::RestartEndsSession,
                        format!("restarting pane process {old} failed: {error:#}"),
                    );
                }
                if let Some(opened) = self.lock().open.get_mut(&pane) {
                    opened.restarting = false;
                }
                return;
            }
        };
        let serial = panes
            .program_serial(pane)
            .or_else(|| simulator::serial_of(pid))
            .unwrap_or(0);
        {
            let mut state = self.lock();
            state.stats.restarted += 1;
            let Some(opened) = state.open.get_mut(&pane) else {
                return;
            };
            opened.pid = pid;
            opened.serial = serial;
            opened.restarting = false;
        }
        event(
            "restart",
            format!(
                "d{driver}: pane {old} restarted as {pid} on {}",
                terminal_of(pid)
            ),
            Tone::Info,
        );
        // 7. The old session ends; the new program leads its own.
        checked(Invariant::RestartEndsSession);
        if let Some(problem) = self.leads_own_session(&panes, pane, pid, serial).await {
            self.violated(
                Invariant::RestartEndsSession,
                format!("restarted pane process {pid} does not lead its own session: {problem}"),
            );
        }
        if !self.settle(|| running_in(&[old_session]).is_empty()).await {
            self.violated(
                Invariant::RestartEndsSession,
                format!(
                    "pane process {old}'s session still runs {:?} after a restart",
                    running_in(&[old_session])
                ),
            );
        }
    }
}

/// Whether `pid`'s terminal shows each of `lines` answered, in order.
fn answered(pid: u32, lines: &[String]) -> bool {
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

/// The client the workload observes panes as.
const OBSERVER: u64 = 1;

/// The client that attaches to panes.
const ATTACHER: u64 = 1;

/// The terminal the process of `serial` started on, while the kernel still has it.
fn terminal_by_serial(serial: u64) -> Option<String> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|process| process.serial == serial)
        .and_then(|process| process.terminal)
}

/// What is wrong with a pane taking in `chunks` from a terminal that gave its master side
/// `written` to read, if anything: each chunk is either the next part of it or one of the
/// runtime's notices, so what the pane took in is a prefix of it (the rest may not have been
/// read yet), notices aside.
fn delivery_problem(written: &[u8], chunks: &[Vec<u8>]) -> Option<String> {
    let mut at = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        if written[at..].starts_with(chunk) {
            at += chunk.len();
        } else if !chunk.starts_with(b"\r\n[bmux] ") {
            let next = &written[at..written.len().min(at + chunk.len().max(16))];
            return Some(format!(
                "chunk {index} of {} took in {:?} where the terminal gave {:?} next (at byte {at} \
                 of {})",
                chunks.len(),
                String::from_utf8_lossy(chunk),
                String::from_utf8_lossy(next),
                written.len()
            ));
        }
    }
    None
}
