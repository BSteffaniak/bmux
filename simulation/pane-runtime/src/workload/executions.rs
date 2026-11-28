//! Program executions through the runtime's execution service, as its clients run them:
//! reserve under a mutation id, launch, wait for the program to exit, reconcile, retire. And
//! the invariants they check: 17 (a mutation's reservation names one execution, and a crashed
//! server's executions are retired) and 18 (the exit code reported is the program's).

use std::sync::{Arc, atomic::Ordering}; // simulation-safe: the run's own server generation

use bmux_pane_runtime_plugin::simulation::Panes;
use sim_support::{invariants::checked, kernel::killed_by_fault};

use super::{Shared, draw, ledger::Launch};
use crate::invariants::Invariant;

/// The program executions run: `exit CODE` exits at once with that code.
const PROGRAM: &str = "exit";

/// The size executions' terminals open at.
const SIZE: (u16, u16) = (24, 80);

/// What an `execute` action does.
enum Choice {
    /// Launch under a new mutation id, running `exit CODE`.
    Launch(u128, u8),
    /// Launch again under a known mutation id, as a client whose reply was lost does.
    Retry(u128, u8),
    /// Finish a launched execution: wait for its exit, then reconcile, retire and close it.
    Settle(u128),
}

// Each locked section is a deliberate unit (see the workload's other `impl Shared`).
#[allow(clippy::significant_drop_tightening)]
impl Shared {
    /// Launch a program execution (or launch one again), or finish one launched earlier.
    pub(super) async fn execute(&self) {
        let generation = self.generation.load(Ordering::SeqCst);
        let (panes, choice) = {
            let mut state = self.lock();
            let pending: Vec<u128> = state
                .executions
                .iter()
                .filter(|(_, launch)| !launch.settled && launch.generation == generation)
                .map(|(mutation, _)| *mutation)
                .collect();
            let known: Vec<(u128, u8)> = state
                .executions
                .iter()
                .map(|(mutation, launch)| (*mutation, launch.code))
                .collect();
            let choice = if !pending.is_empty() && draw(2) == 0 {
                Choice::Settle(pending[index(pending.len())])
            } else if !known.is_empty() && draw(3) == 0 {
                let (mutation, code) = known[index(known.len())];
                Choice::Retry(mutation, code)
            } else {
                state.mutations += 1;
                let code = u8::try_from(draw(256)).unwrap_or_default();
                Choice::Launch(u128::from(state.mutations), code)
            };
            (Arc::clone(&state.panes), choice)
        };
        match choice {
            Choice::Launch(mutation, code) => {
                self.launch(&panes, mutation, code, generation, false).await;
            }
            Choice::Retry(mutation, code) => {
                self.launch(&panes, mutation, code, generation, true).await;
            }
            Choice::Settle(mutation) => self.settle_execution(&panes, mutation).await,
        }
    }

    /// Reserve and launch `exit code` under `mutation` (again, if `retry`), checking that the
    /// reservation names the execution it always did, and no other mutation's (invariant 17).
    async fn launch(&self, panes: &Panes, mutation: u128, code: u8, generation: u64, retry: bool) {
        let launched = panes
            .execute(mutation, PROGRAM, vec![code.to_string()], SIZE)
            .await;
        let receipt = match launched {
            Ok(receipt) => receipt,
            // A retry asks for what the first launch did: the service must not call it a
            // conflict. (Other refusals come from faults: a failed spawn or file operation.)
            Err(problem) if retry && problem.contains("Conflict") => {
                self.violated(
                    Invariant::Reservations,
                    format!("retrying execution mutation {mutation} was refused: {problem}"),
                );
                return;
            }
            Err(_) => return,
        };
        // 17. A mutation's reservation names one execution, the same on every retry, and no
        // other mutation's.
        checked(Invariant::Reservations);
        let mut state = self.lock();
        let shared = state
            .executions
            .iter()
            .find(|(other, launch)| {
                **other != mutation && launch.execution.as_ref() == Some(&receipt.execution)
            })
            .map(|(other, _)| *other);
        if let Some(other) = shared {
            let message = format!(
                "execution mutations {other} and {mutation} were given the same execution"
            );
            super::violated(&mut state.violations, Invariant::Reservations, message);
        }
        let launch = state.executions.entry(mutation).or_insert(Launch {
            code,
            execution: None,
            generation,
            settled: false,
        });
        match &launch.execution {
            Some(execution) if *execution != receipt.execution => {
                let message = format!(
                    "retrying execution mutation {mutation} gave another execution: \
                     {execution:?}, then {:?}",
                    receipt.execution
                );
                super::violated(&mut state.violations, Invariant::Reservations, message);
            }
            Some(_) => {}
            None => launch.execution = Some(receipt.execution),
        }
    }

    /// Finish `mutation`'s execution: wait for its program to exit, check the exit code the
    /// runtime and the reconciled receipt report (invariant 18), then retire it and close its
    /// pane, as a client done with it does.
    pub(super) async fn settle_execution(&self, panes: &Panes, mutation: u128) {
        let generation = self.generation.load(Ordering::SeqCst);
        let Some((execution, code)) = self.lock().executions.get_mut(&mutation).and_then(|launch| {
            launch.settled = true;
            launch.execution.clone().map(|execution| (execution, launch.code))
        }) else {
            return;
        };
        self.settle(|| {
            panes
                .execution_status(&execution)
                .is_none_or(|status| status.ended || status.error.is_some())
        })
        .await;
        // 18. The exit code reported is the program's (unless a signal or a fault ended it, or
        // a crash or power cut did: on Windows a process ended so exits with code 1).
        let serial = panes.program_serial(execution.pane());
        let interfered = || {
            self.generation.load(Ordering::SeqCst) != generation
                || serial.is_some_and(killed_by_fault)
        };
        let status = panes.execution_status(&execution);
        if let Some(status) = &status
            && !interfered()
            && status.ended
            && status.signal.is_none()
            && status.error.is_none()
        {
            checked(Invariant::ExitCode);
            if status.exit_code != Some(u32::from(code)) {
                self.violated(
                    Invariant::ExitCode,
                    format!(
                        "an execution of `{PROGRAM} {code}` reported exit code {:?}",
                        status.exit_code
                    ),
                );
            }
            if let Ok(receipt) = panes.reconcile(&execution).await
                && !interfered()
                && receipt.exit_signal.is_none()
                && receipt.error.is_none()
                && receipt.exit_code != Some(u32::from(code))
            {
                self.violated(
                    Invariant::ExitCode,
                    format!(
                        "an execution of `{PROGRAM} {code}` was reconciled to exit code {:?}",
                        receipt.exit_code
                    ),
                );
            }
        }
        // Retiring needs its capture complete, which faults can deny: then it stays reserved.
        let _ = panes.retire(&execution).await;
        if panes.panes().contains(&execution.pane()) {
            let _ = panes.close(execution.pane()).await;
        }
    }

    /// Finish every execution the current server launched (at the run's end).
    pub(super) async fn settle_executions(&self) {
        let generation = self.generation.load(Ordering::SeqCst);
        loop {
            let (panes, next) = {
                let state = self.lock();
                let next = state
                    .executions
                    .iter()
                    .find(|(_, launch)| !launch.settled && launch.generation == generation)
                    .map(|(mutation, _)| *mutation);
                (Arc::clone(&state.panes), next)
            };
            let Some(mutation) = next else {
                return;
            };
            self.settle_execution(&panes, mutation).await;
        }
    }

    /// After a crash, the executions the dead server launched and no client finished: the new
    /// server's activation retires them (their sessions are swept), and never reports one
    /// running (invariant 17).
    pub(super) async fn check_orphaned_executions(&self) {
        let generation = self.generation.load(Ordering::SeqCst);
        let (panes, orphans) = {
            let mut state = self.lock();
            let orphans: Vec<(u128, bmux_pane_runtime_plugin::simulation::Execution)> = state
                .executions
                .iter_mut()
                .filter(|(_, launch)| !launch.settled && launch.generation != generation)
                .filter_map(|(mutation, launch)| {
                    launch.settled = true;
                    launch.execution.clone().map(|execution| (*mutation, execution))
                })
                .collect();
            (Arc::clone(&state.panes), orphans)
        };
        // A failing disk may keep the store from recording what it retired.
        if self.profile.faults.file != 0 {
            return;
        }
        for (mutation, execution) in orphans {
            checked(Invariant::Reservations);
            match panes.reconcile(&execution).await {
                Ok(receipt) if matches!(receipt.phase.as_str(), "Launching" | "Running") => {
                    self.violated(
                        Invariant::Reservations,
                        format!(
                            "after a crash, execution mutation {mutation} is still {} \
                             (receipt {receipt:?})",
                            receipt.phase
                        ),
                    );
                }
                _ => {}
            }
        }
    }
}

/// A drawn index below `len`.
fn index(len: usize) -> usize {
    usize::try_from(draw(len as u64)).unwrap_or(0)
}
