//! Pane terminal sessions on `switchy_pty`'s simulated kernel (the `simulator` feature).
//!
//! The authority the native backend of the run's personality has:
//!
//! * **Unix** (Linux, macOS): the pane's program leads its own session, which is swept of every
//!   member after it ends, and each live session leaves a crash record that a later activation
//!   recovers. A simulated process's serial stands in for its start time. There is no crash
//!   guard: a simulated server cannot die out from under its processes.
//! * **Windows**: the pane's processes form a kill-on-close job (the kernel's session: a Windows
//!   process cannot leave the console session it was created in, so its members are the job's,
//!   descendants that outlived their parent included). There is no cooperative stop and no crash
//!   record; releasing the job ends what is left unless its survivors were released first.

use std::path::{Path, PathBuf};
use std::time::Duration;
use switchy_thread::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use switchy_pty::{
    Signal,
    simulator::{self, ProcessState},
};

/// How long a sweep waits for killed members to exit, in simulated time.
const SWEEP_BUDGET: Duration = Duration::from_secs(5);

/// Windows: the job the program is spawned into (by its session's serial, 0 before the
/// spawn), from its creation, as the native backend's job handle is: dropping the preparation
/// before [`Session::adopt`] takes the job over closes it, ending its processes.
#[derive(Debug, Default)]
pub(crate) struct Preparation {
    job: AtomicU64,
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the Windows backend, which can fail"
)]
pub(crate) fn prepare() -> anyhow::Result<Preparation> {
    Ok(Preparation::default())
}

impl Preparation {
    pub(crate) fn spawn(
        &self,
        slave: &bmux_pty::PtySlave,
        command: &bmux_pty::SpawnCommand,
    ) -> std::io::Result<Box<dyn crate::runtime::pty::Child + Send + Sync>> {
        let child = slave.spawn(command)?;
        if super::windows()
            && let Some(serial) = simulator::snapshot()
                .processes
                .into_iter()
                .find(|process| process.pid == child.id())
                .map(|process| process.session_serial)
        {
            self.job.store(serial, Ordering::SeqCst);
        }
        Ok(Box::new(child))
    }

    /// The job, taken over (the preparation no longer closes it).
    fn take_job(&self) -> Option<u64> {
        Some(self.job.swap(0, Ordering::SeqCst)).filter(|job| *job != 0)
    }
}

impl Drop for Preparation {
    fn drop(&mut self) {
        // The job's handle closes: kill-on-close ends what runs in it.
        if let Some(job) = self.take_job() {
            for pid in job_members(job) {
                let _ = simulator::signal_process(pid, Signal::Kill);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    sid: u32,
    start: Option<crate::platform::ProcessStartIdentity>,
    /// The Windows job's semantics, rather than a Unix session's: the job, by its session's
    /// serial (a job is a handle, never reused, unlike a session id).
    job: Option<u64>,
    /// Windows: releasing the job ends its processes.
    kill_on_close: AtomicBool,
    /// Windows: the job handle was released.
    released: AtomicBool,
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

/// The running members of the job whose session has serial `serial`.
fn job_members(serial: u64) -> Vec<u32> {
    simulator::snapshot()
        .processes
        .into_iter()
        .filter(|process| {
            process.session_serial == serial && process.state == ProcessState::Running
        })
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

/// Terminate every member of the job `serial`, and wait (in simulated time) until none runs.
fn sweep_job(serial: u64) -> std::io::Result<()> {
    let deadline = switchy_time::Instant::now() + SWEEP_BUDGET;
    loop {
        let survivors = job_members(serial);
        if survivors.is_empty() {
            return Ok(());
        }
        if switchy_time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{} process(es) remain in the pane's job after termination",
                    survivors.len()
                ),
            ));
        }
        for pid in survivors {
            // A member that exited meanwhile is already gone.
            let _ = simulator::signal_process(pid, Signal::Kill);
        }
        switchy_thread::thread::sleep(Duration::from_millis(10));
    }
}

impl Session {
    pub(crate) fn adopt(
        preparation: Preparation,
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
            start: crate::platform::process_start_identity(leader),
            job: preparation.take_job(),
            kill_on_close: AtomicBool::new(true),
            released: AtomicBool::new(false),
        })
    }

    /// Windows: operations on a released job fail, as on a closed handle.
    fn open(&self) -> std::io::Result<()> {
        if self.job.is_some() && self.released.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("pane job already released"));
        }
        Ok(())
    }

    pub(crate) fn commit_record(
        &self,
        root: &Path,
        token: uuid::Uuid,
    ) -> anyhow::Result<Option<PathBuf>> {
        // The job's kill-on-close covers a crashed server on Windows; nothing to record.
        if self.job.is_some() {
            return Ok(None);
        }
        switchy_fs::sync::create_dir_all(root)?;
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

    pub(crate) fn terminate_leader(&self, leader: u32) -> std::io::Result<bool> {
        // Cooperative stop is unsupported on Windows; the caller escalates.
        if self.job.is_some() {
            return Ok(false);
        }
        match simulator::signal_process(leader, Signal::Terminate) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// The leader's serial: its start identity (Unix), or its job's (Windows).
    pub(crate) fn leader_serial(&self) -> Option<u64> {
        self.start.map(|start| start.0).or(self.job)
    }

    pub(crate) fn sweep(&self) -> std::io::Result<()> {
        self.open()?;
        match self.job {
            Some(job) => sweep_job(job),
            None if self.reused() => Ok(()),
            None => sweep_session(self.sid),
        }
    }

    pub(crate) fn occupied(&self) -> std::io::Result<bool> {
        self.open()?;
        Ok(!match self.job {
            Some(job) => job_members(job),
            None if self.reused() => Vec::new(),
            None => members(self.sid),
        }
        .is_empty())
    }

    /// Unix: whether the session ended and its id now names another (as the Unix backend).
    fn reused(&self) -> bool {
        self.start.is_some_and(|start| {
            crate::platform::process_start_identity(self.sid)
                .is_some_and(|current| current != start)
        })
    }

    /// Windows: clear kill-on-close so releasing the job leaves its processes running.
    pub(crate) fn release_survivors(&self) {
        self.kill_on_close.store(false, Ordering::SeqCst);
    }

    /// Windows: close the job, ending its processes unless released. Unix: nothing to release
    /// (there is no crash guard in simulation).
    pub(crate) fn release(&self) {
        let Some(job) = self.job else {
            return;
        };
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.kill_on_close.load(Ordering::SeqCst) {
            for pid in job_members(job) {
                let _ = simulator::signal_process(pid, Signal::Kill);
            }
        }
    }
}

/// Closing a Windows job handle, as the operating system does for a server that dies, ends its
/// processes unless they were released.
impl Drop for Session {
    fn drop(&mut self) {
        self.release();
    }
}

/// End every member of the session `leader` started, which the server could not adopt (Unix;
/// on Windows the job closed with the preparation, ending its members). Call it before
/// reaping the leader.
pub(crate) fn sweep_unadopted(leader: u32) -> std::io::Result<()> {
    if super::windows() || leader <= 1 {
        return Ok(());
    }
    sweep_session(leader)
}

/// The simulated server has no process of its own to name (a crash is modelled by
/// [`crate::runtime::session_control::SessionAuthority::abandon`] in the same process), so its
/// records are swept whenever their session is still theirs.
pub(crate) fn recover_record(path: &Path) -> anyhow::Result<crate::platform::RecordRecovery> {
    let record: Record = serde_json::from_slice(&switchy_fs::sync::read(path)?)?;
    anyhow::ensure!(
        record.version == RECORD_VERSION,
        "unsupported pane session record version {}",
        record.version
    );
    anyhow::ensure!(record.sid > 1, "invalid pane session record");
    let leader = u32::try_from(record.sid)?;
    let recorded = record
        .leader_start
        .map(|(serial, fraction)| crate::platform::ProcessStartIdentity(serial, fraction));
    if still_ours(leader, recorded) {
        sweep_session(leader)?;
    }
    Ok(crate::platform::RecordRecovery::Swept)
}

/// Whether session `leader` is still the one recorded: its leader runs with the recorded serial,
/// or (the leader gone) every remaining member started after it.
fn still_ours(leader: u32, recorded: Option<crate::platform::ProcessStartIdentity>) -> bool {
    let Some(recorded) = recorded else {
        // Without a start identity there is no way to rule out reuse.
        return false;
    };
    if let Some(current) = crate::platform::process_start_identity(leader)
        && session_of(leader).is_some()
    {
        return session_of(leader) == Some(leader) && current == recorded;
    }
    members(leader).into_iter().all(|pid| {
        crate::platform::process_start_identity(pid)
            .is_some_and(|start| (start.0, start.1) >= (recorded.0, recorded.1))
    })
}
