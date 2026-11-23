//! The actions on the whole server, which take it to themselves (see [`Kind::exclusive`](crate::profile::Kind::exclusive)): a
//! crash, a power loss and a pane runtime activated beside it; and the final check that
//! nothing is left.

use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use bmux_pane_runtime_plugin::simulation::Panes;
use bmux_simvar_tui::Tone;
use sim_support::invariants::checked;
use switchy_pty::simulator::{self, ProcessState};

use super::{
    SHELL, Shared, draw, event,
    kernel::{alive, detached, killed_by_fault, live_threads, running_in},
    ledger::{State, Witness},
};
use crate::{invariants::Invariant, telemetry::Board};

/// The drivers paused for an exclusive action (see [`Shared::pause`]): they start nothing new
/// until it is resumed or dropped, and no other exclusive action begins until it is dropped.
struct Paused<'a> {
    shared: &'a Shared,
    resumed: bool,
    _one_at_a_time: tokio::sync::MutexGuard<'a, ()>,
}

impl Paused<'_> {
    /// Wait until no action is under way (each ends, or is cancelled by a crash).
    async fn drained(&self) {
        while self.shared.lock().busy > 0 {
            switchy_async::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Let drivers start actions again (with `state`, the shared state, locked), while this
    /// exclusive action goes on checking what it did.
    const fn resume(&mut self, state: &mut State) {
        state.paused = false;
        self.resumed = true;
    }
}

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        if !self.resumed {
            self.shared.lock().paused = false;
        }
    }
}

// Each locked section is a deliberate unit (see the workload's other `impl Shared`).
#[allow(clippy::significant_drop_tightening)]
impl Shared {
    /// Pause the drivers for an exclusive action (see [`Kind::exclusive`](crate::profile::Kind::exclusive)), one at a time, if
    /// `wanted` (given the state) says there is anything for it to do. Returns the pause, which
    /// lets drivers go on when dropped (or [`Paused::resume`]d), and the server's panes.
    async fn pause(&self, wanted: impl FnOnce(&State) -> bool) -> Option<(Paused<'_>, Arc<Panes>)> {
        let one_at_a_time = self.exclusive.lock().await;
        let mut state = self.lock();
        if !wanted(&state) {
            return None;
        }
        state.paused = true;
        let panes = Arc::clone(&state.panes);
        drop(state);
        let paused = Paused {
            shared: self,
            resumed: false,
            _one_at_a_time: one_at_a_time,
        };
        Some((paused, panes))
    }

    /// Activate a second pane runtime on the server's runtime directory while the server runs
    /// (as a command run in a client does), checking invariant 12: it kills nothing the server
    /// runs. Drivers pause meanwhile, so no close or restart of theirs kills a program.
    pub(super) async fn bystander(&self, driver: u64) {
        let Some((paused, panes)) = self.pause(|state| !state.open.is_empty()).await else {
            return;
        };
        paused.drained().await;
        let message = format!("d{driver}: a command activated the pane runtime beside the server");
        event("bystander", message, Tone::Info);
        // The programs of the server's live panes.
        let programs: Vec<u64> = self
            .lock()
            .open
            .keys()
            .filter_map(|pane| panes.program_serial(*pane))
            .filter(|serial| alive(*serial).is_some())
            .collect();
        let before = simulator::signal_log().len();
        let bystander = Panes::recovering(SHELL, &self.records);
        bystander.shut_down();
        drop(bystander);
        // 12. It killed none of them (the kill fault aside).
        checked(Invariant::BystanderHarmless);
        let killed: Vec<u64> = simulator::signal_log()
            .get(before..)
            .unwrap_or_default()
            .iter()
            .filter(|record| {
                record.signal == switchy_pty::Signal::Kill
                    && !record.by_fault
                    && programs.contains(&record.serial)
            })
            .map(|record| record.serial)
            .collect();
        if !killed.is_empty() {
            self.violated(
                Invariant::BystanderHarmless,
                format!(
                    "a pane runtime activated beside the running server killed its panes' \
                 programs {killed:?}"
                ),
            );
        }
        drop(paused);
    }

    /// Crash the server, start processes it never owned, then start a new server on the same
    /// runtime directory, checking invariant 6.
    pub(super) async fn crash(&self, driver: u64, power_loss: bool) {
        let Some((mut paused, panes)) = self
            .pause(|state| !state.open.is_empty() || state.opening != 0)
            .await
        else {
            return;
        };
        if power_loss {
            event("power", format!("d{driver}: the power failed"), Tone::Warn);
        } else {
            event(
                "crash",
                format!("d{driver}: the server crashed"),
                Tone::Warn,
            );
        }
        // The drivers' work under way stops with the server.
        self.generation.fetch_add(1, Ordering::SeqCst);
        paused.drained().await;
        // Die: every task and thread of the server stops where it is.
        panes.crash();
        drop(panes);
        let filesystem = switchy_fs::simulator::current_filesystem();
        if power_loss {
            // Every process dies, and the disk keeps what was made durable and a random part
            // of the rest.
            simulator::kill_all();
            self.lock().witnesses.clear();
            filesystem.crash(&switchy_fs::simulator::CrashPolicy::power_loss());
        } else {
            // The dead server's open files close; what it wrote stays.
            filesystem.crash(&switchy_fs::simulator::CrashPolicy::process_crash());
        }
        // Everything running now that no outsider started belongs to the dead server.
        // (So do the daemons panes' programs detached, by design.)
        let mut outsiders: Vec<u64> = self.lock().witnesses.iter().map(|w| w.serial).collect();
        outsiders.extend(detached());
        let owned: Vec<u64> = simulator::snapshot()
            .processes
            .iter()
            .filter(|p| p.state == ProcessState::Running && !outsiders.contains(&p.serial))
            .map(|p| p.session_serial)
            .collect();
        // Processes started while the crashed sessions linger (their ids may be reused).
        let mut witnesses = Vec::new();
        for _ in 0..=draw(2) {
            let Ok(pair) = switchy_pty::PtyPair::open(switchy_pty::PtySize::new(80, 24)) else {
                continue;
            };
            let Ok(child) = pair
                .slave
                .spawn(switchy_pty::SpawnCommand::new("sleep").arg("3600000"))
            else {
                continue;
            };
            let terminal = simulator::process_io(child.id())
                .and_then(|io| io.terminal)
                .unwrap_or_default();
            if let Some(serial) = simulator::serial_of(child.id()) {
                witnesses.push(Witness {
                    serial,
                    terminal,
                    _child: child,
                    _pair: pair,
                });
            }
        }
        let fresh = Arc::new(Panes::recovering(SHELL, &self.records));
        fresh.set_sweep_on_exit(self.profile.sweep);
        let outside: Vec<u64> = {
            let mut state = self.lock();
            for pane in state.open.keys() {
                Board::closed(&format!("{pane:?}"));
            }
            state.open.clear();
            state.opening = 0;
            state.panes = fresh;
            paused.resume(&mut state);
            state.stats.crashed += 1;
            state.witnesses.extend(witnesses);
            state.witnesses.iter().map(|w| w.serial).collect()
        };
        // 6. Nothing the crashed server owned runs; everything else does.
        checked(Invariant::CrashSessionsEnd);
        // (Programs the dead server's closed terminals hung up on take a moment to end.)
        let ended = self.settle(|| running_in(&owned).is_empty()).await;
        let survivors = running_in(&owned);
        if !ended {
            self.violated(
                Invariant::CrashSessionsEnd,
                format!("the crashed server's sessions still run {survivors:?} after recovery"),
            );
        }
        let running: Vec<u64> = simulator::snapshot()
            .processes
            .iter()
            .filter(|p| p.state == ProcessState::Running)
            .map(|p| p.serial)
            .collect();
        for serial in outside {
            if !running.contains(&serial) && !killed_by_fault(serial) {
                self.violated(
                    Invariant::CrashSessionsEnd,
                    format!("recovery ended process serial {serial} it never owned"),
                );
            }
        }
    }

    /// Invariant 9: with every pane closed, nothing the runtime had is left: no terminal's
    /// master side open, no process running but those started outside it, no session record.
    pub(super) async fn check_nothing_leaks(&self) {
        checked(Invariant::NothingLeft);
        let mut outsiders: Vec<u64> = self.lock().witnesses.iter().map(|w| w.serial).collect();
        outsiders.extend(detached());
        let strays = || -> Vec<String> {
            simulator::snapshot()
                .processes
                .into_iter()
                .filter(|p| p.state == ProcessState::Running && !outsiders.contains(&p.serial))
                .map(|p| format!("{} ({})", p.pid, p.program))
                .collect()
        };
        // Processes a close or a crash ended may take a moment to exit.
        if !self.settle(|| strays().is_empty()).await {
            self.violated(
                Invariant::NothingLeft,
                format!("processes still run with every pane closed: {:?}", strays()),
            );
        }
        let outsider_terminals: Vec<String> = self
            .lock()
            .witnesses
            .iter()
            .map(|w| w.terminal.clone())
            .collect();
        let open = || -> Vec<String> {
            simulator::snapshot()
                .terminals
                .into_iter()
                // The outsiders' own terminals stay open until the run ends.
                .filter(|t| t.master_handles > 0 && !outsider_terminals.contains(&t.name))
                .map(|t| format!("{} ({} master handles)", t.name, t.master_handles))
                .collect()
        };
        if !self.settle(|| open().is_empty()).await {
            self.violated(
                Invariant::NothingLeft,
                format!(
                    "terminals' master sides still open with every pane closed: {:?} (threads: \
                 {:?})",
                    open(),
                    live_threads()
                ),
            );
        }
        let records: Vec<String> = switchy_fs::sync::read_dir_sorted(&self.records)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        if !records.is_empty() && self.profile.faults.file == 0 {
            self.violated(
                Invariant::NothingLeft,
                format!("session records left with every pane closed: {records:?}"),
            );
        }
    }
}
