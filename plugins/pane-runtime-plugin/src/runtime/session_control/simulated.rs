//! Pane terminal sessions on `switchy_pty`'s simulated kernel (the `simulator` feature).
//!
//! The same authority as the Unix backend: the pane's program leads its own session, which is
//! swept of every member after it ends, and each live session leaves a crash record that a
//! later activation recovers. Processes come from the simulated kernel; a simulated process's
//! serial stands in for its start time. There is no crash guard: a simulated server cannot die
//! out from under its processes.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use switchy_pty::{
    Signal,
    simulator::{self, ProcessState},
};

/// How long a sweep waits for killed members to exit, in simulated time.
const SWEEP_BUDGET: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(in crate::runtime) struct Preparation;

#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the Windows backend, which can fail"
)]
pub(super) fn prepare() -> anyhow::Result<Preparation> {
    Ok(Preparation)
}

impl Preparation {
    #[expect(
        clippy::unused_self,
        reason = "the Windows preparation carries the job to spawn into"
    )]
    pub(in crate::runtime) fn spawn(
        &self,
        slave: &bmux_pty::PtySlave,
        command: bmux_pty::SpawnCommand,
    ) -> std::io::Result<Box<dyn crate::runtime::pty::Child + Send + Sync>> {
        crate::runtime::pty::Slave::spawn_command(slave, command)
    }
}

#[derive(Debug)]
pub(super) struct Session {
    sid: u32,
    start: Option<crate::runtime::ProcessStartIdentity>,
}

const RECORD_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    version: u32,
    sid: i32,
    leader_start: Option<(u64, u64)>,
}

/// The session `pid` belongs to, while it runs.
fn session_of(pid: u32) -> Option<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .find(|process| process.pid == pid && process.state == ProcessState::Running)
        .map(|process| process.session)
}

/// The running members of session `sid`.
fn members(sid: u32) -> Vec<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|process| process.session == sid && process.state == ProcessState::Running)
        .map(|process| process.pid)
        .collect()
}

/// Kill every member of `sid`, and wait (in simulated time) until none runs.
fn sweep_session(sid: u32) -> std::io::Result<()> {
    if sid <= 1 {
        return Err(std::io::Error::other(
            "refusing to sweep an invalid session",
        ));
    }
    let deadline = switchy_time::Instant::now() + SWEEP_BUDGET;
    loop {
        let survivors = members(sid);
        if survivors.is_empty() {
            return Ok(());
        }
        if switchy_time::Instant::now() >= deadline {
            return Err(std::io::Error::other(format!(
                "session {sid} still has members after the sweep: {survivors:?}"
            )));
        }
        for pid in survivors {
            // A member that exited meanwhile is already gone.
            let _ = simulator::signal_process(pid, Signal::Kill);
        }
        switchy_thread::thread::sleep(Duration::from_millis(10));
    }
}

impl Session {
    pub(super) fn adopt(
        _preparation: Preparation,
        leader: u32,
        _master: &bmux_pty::PtyMaster,
    ) -> anyhow::Result<Self> {
        let actual = session_of(leader);
        anyhow::ensure!(
            actual == Some(leader),
            "pane process is not its own session leader (session {actual:?})"
        );
        Ok(Self {
            sid: leader,
            start: crate::runtime::process_start_identity_for_pid(leader),
        })
    }

    pub(super) fn commit_record(
        &self,
        root: &Path,
        token: uuid::Uuid,
    ) -> anyhow::Result<Option<PathBuf>> {
        std::fs::create_dir_all(root)?;
        let path = root.join(format!("{token}.json"));
        crate::atomic_state::commit(
            &path,
            &Record {
                version: RECORD_VERSION,
                sid: i32::try_from(self.sid)?,
                leader_start: self.start.map(|start| (start.0, start.1)),
            },
        )?;
        Ok(Some(path))
    }

    #[expect(clippy::unused_self, reason = "same signature as the Windows backend")]
    pub(super) fn terminate_leader(&self, leader: u32) -> std::io::Result<bool> {
        match simulator::signal_process(leader, Signal::Terminate) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub(super) fn sweep(&self) -> std::io::Result<()> {
        sweep_session(self.sid)
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "same signature as the Unix backend, whose discovery can fail"
    )]
    pub(super) fn occupied(&self) -> std::io::Result<bool> {
        Ok(!members(self.sid).is_empty())
    }

    #[expect(
        clippy::unused_self,
        reason = "Windows needs the session to release its job"
    )]
    pub(super) const fn release_survivors(&self) {}

    #[expect(
        clippy::unused_self,
        reason = "no crash guard to release in simulation"
    )]
    pub(super) const fn release(&self) {}
}

pub(super) fn recover_record(path: &Path) -> anyhow::Result<()> {
    let record: Record = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
        record.version == RECORD_VERSION,
        "unsupported pane session record version {}",
        record.version
    );
    anyhow::ensure!(record.sid > 1, "invalid pane session record");
    let leader = u32::try_from(record.sid)?;
    let recorded = record
        .leader_start
        .map(|(serial, fraction)| crate::runtime::ProcessStartIdentity(serial, fraction));
    if still_ours(leader, recorded) {
        sweep_session(leader)?;
    }
    Ok(())
}

/// Whether session `leader` is still the one recorded: its leader runs with the recorded serial,
/// or (the leader gone) every remaining member started after it.
fn still_ours(leader: u32, recorded: Option<crate::runtime::ProcessStartIdentity>) -> bool {
    let Some(recorded) = recorded else {
        // Without a start identity there is no way to rule out reuse.
        return false;
    };
    if let Some(current) = crate::runtime::process_start_identity_for_pid(leader)
        && session_of(leader).is_some()
    {
        return session_of(leader) == Some(leader) && current == recorded;
    }
    members(leader).into_iter().all(|pid| {
        crate::runtime::process_start_identity_for_pid(pid)
            .is_some_and(|start| (start.0, start.1) >= (recorded.0, recorded.1))
    })
}
