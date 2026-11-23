//! Unix session authority: the leader's session ID is the pane's identity.
use super::session_sweep;
use serde::{Deserialize, Serialize};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use switchy_thread::sync::Mutex;

/// Nothing has to exist before a Unix spawn: `setsid` happens in the child.
#[derive(Debug)]
pub(crate) struct Preparation;

#[expect(
    clippy::unnecessary_wraps,
    reason = "same signature as the Windows backend, which can fail"
)]
pub(crate) fn prepare() -> anyhow::Result<Preparation> {
    Ok(Preparation)
}

impl Preparation {
    /// Spawn the pane's program as the leader of a new session on `slave`.
    #[expect(
        clippy::unused_self,
        reason = "the Windows preparation carries the job to spawn into"
    )]
    pub(crate) fn spawn(
        &self,
        slave: &bmux_pty::PtySlave,
        command: &bmux_pty::SpawnCommand,
    ) -> std::io::Result<Box<dyn crate::runtime::pty::Child + Send + Sync>> {
        Ok(Box::new(slave.spawn(command)?))
    }
}

#[derive(Debug)]
pub(crate) struct Session {
    sid: i32,
    start: Option<crate::platform::ProcessStartIdentity>,
    guard: Mutex<Option<super::crash_guard::Registration>>,
}

/// Version 2 names the server that wrote the record.
const RECORD_VERSION: u32 = 2;

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    version: u32,
    sid: i32,
    /// The leader's start identity; `None` when the platform reports none.
    leader_start: Option<(u64, u64)>,
    /// The server process that recorded the session: its pid and start identity. Absent
    /// from version 1 records.
    #[serde(default)]
    owner: Option<(u32, (u64, u64))>,
}

/// This process, as a record's owner.
fn this_process() -> Option<(u32, (u64, u64))> {
    let pid = crate::platform::files::process_id();
    crate::platform::process_start_identity(pid).map(|start| (pid, (start.0, start.1)))
}

impl Session {
    pub(crate) fn adopt(
        _preparation: Preparation,
        leader: u32,
        master: &bmux_pty::PtyMaster,
    ) -> anyhow::Result<Self> {
        let sid = i32::try_from(leader)?;
        // SAFETY: getsid has no memory-safety preconditions.
        let actual = unsafe { libc::getsid(sid) };
        anyhow::ensure!(
            actual == sid,
            "pane process is not its own session leader (session {actual})"
        );
        let start = crate::platform::process_start_identity(leader);
        let guard = super::crash_guard::register(sid, Some(master.as_raw_fd()));
        Ok(Self {
            sid,
            start,
            guard: Mutex::new(guard),
        })
    }

    pub(crate) fn commit_record(
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
                sid: self.sid,
                leader_start: self.start.map(|start| (start.0, start.1)),
                owner: this_process(),
            },
        )?;
        Ok(Some(path))
    }

    #[expect(clippy::unused_self, reason = "same signature as the Windows backend")]
    pub(crate) fn terminate_leader(&self, leader: u32) -> std::io::Result<bool> {
        let pid = libc::pid_t::try_from(leader).map_err(std::io::Error::other)?;
        // SAFETY: the caller holds the session lock and checked that the leader
        // has not exited, so the pid is still our unreaped child.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        Err(error)
    }

    pub(crate) fn sweep(&self) -> std::io::Result<()> {
        if self.reused() {
            return Ok(());
        }
        session_sweep::sweep(self.sid)
    }

    pub(crate) fn occupied(&self) -> std::io::Result<bool> {
        if self.reused() {
            return Ok(false);
        }
        session_sweep::occupied(self.sid)
    }

    /// Whether the session ended and its id now names another: a process with the leader's
    /// pid runs with another start. (While anything of the session lives, its id is never
    /// handed out again; once it is empty, the id is free.)
    fn reused(&self) -> bool {
        let (Some(start), Ok(leader)) = (self.start, u32::try_from(self.sid)) else {
            return false;
        };
        crate::platform::process_start_identity(leader).is_some_and(|current| current != start)
    }

    #[expect(
        clippy::unused_self,
        reason = "Windows needs the session to release its job"
    )]
    pub(crate) const fn release_survivors(&self) {}

    pub(crate) fn release(&self) {
        if let Ok(mut guard) = self.guard.lock() {
            guard.take();
        }
    }
}

/// End every member of the session `leader` started, which the server could not adopt (its
/// leader had already exited, leaving members that would otherwise outlive the pane). Call it
/// before reaping the leader: its zombie keeps the session's id from being reused.
pub(crate) fn sweep_unadopted(leader: u32) -> std::io::Result<()> {
    super::session_sweep::sweep(
        i32::try_from(leader).map_err(|_| std::io::Error::other("pid out of range"))?,
    )
}

/// Sweep the session one crash record describes, if the server that recorded it is gone.
///
/// Any process activating the pane runtime for this runtime directory recovers its records:
/// the next server after a crash, but also a command run in a client or the CLI while the
/// server runs. A record whose server still runs is left alone, and so is a version 1 record
/// (which names no server) whose session still lives: its server may well be running.
pub(crate) fn recover_record(path: &Path) -> anyhow::Result<crate::platform::RecordRecovery> {
    use crate::platform::RecordRecovery;
    let record: Record = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(
        matches!(record.version, 1 | RECORD_VERSION),
        "unsupported pane session record version {}",
        record.version
    );
    anyhow::ensure!(record.sid > 1, "invalid pane session record");
    let owner_running = record.owner.is_none_or(|(pid, (seconds, fraction))| {
        crate::platform::process_start_identity(pid)
            == Some(crate::platform::ProcessStartIdentity(seconds, fraction))
    });
    if record.owner.is_some() && owner_running {
        return Ok(RecordRecovery::Owned);
    }
    let leader = u32::try_from(record.sid)?;
    let recorded = record
        .leader_start
        .map(|(seconds, fraction)| crate::platform::ProcessStartIdentity(seconds, fraction));
    if !still_ours(leader, recorded) {
        return Ok(RecordRecovery::Swept);
    }
    if owner_running {
        // A server that may still run keeps a session that still has members.
        return Ok(if session_sweep::occupied(record.sid)? {
            RecordRecovery::Owned
        } else {
            RecordRecovery::Swept
        });
    }
    session_sweep::sweep(record.sid)?;
    Ok(RecordRecovery::Swept)
}

/// Whether the recorded session still belongs to the pane that wrote it.
///
/// A live process whose pid equals the session ID must be the recorded leader.
/// Otherwise the number was reused by an unrelated session leader. When the
/// leader is gone, its session ID stays reserved while any member lives, so
/// remaining members are ours; members that started before the recorded leader
/// cannot be, and their presence means the record is stale.
fn still_ours(leader: u32, recorded: Option<crate::platform::ProcessStartIdentity>) -> bool {
    let Some(recorded) = recorded else {
        // Without a start identity there is no way to rule out reuse.
        return false;
    };
    if let Some(current) = crate::platform::process_start_identity(leader) {
        // SAFETY: getsid has no memory-safety preconditions.
        let leads = unsafe { libc::getsid(libc::pid_t::try_from(leader).unwrap_or(0)) }
            == libc::pid_t::try_from(leader).unwrap_or(-1);
        return leads && current == recorded;
    }
    let Ok(sid) = i32::try_from(leader) else {
        return false;
    };
    let Ok(members) = session_sweep::member_pids(sid) else {
        return false;
    };
    members.into_iter().all(|pid| {
        u32::try_from(pid)
            .ok()
            .and_then(crate::platform::process_start_identity)
            .is_some_and(|start| (start.0, start.1) >= (recorded.0, recorded.1))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_without_a_start_identity_are_never_swept() {
        assert!(!still_ours(424_242, None));
    }

    #[test]
    fn a_live_leader_with_a_different_start_is_not_ours() {
        let me = std::process::id();
        let mine = crate::platform::process_start_identity(me).unwrap();
        let other = crate::platform::ProcessStartIdentity(mine.0.wrapping_add(1), mine.1);
        assert!(!still_ours(me, Some(other)));
    }

    #[test]
    fn recovery_sweeps_a_recorded_session() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("/bin/sh");
        leader.args(["-c", "trap '' HUP TERM; sleep 60"]);
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            leader.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = leader.spawn().unwrap();
        let pid = child.id();
        let start = crate::platform::process_start_identity(pid).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        let record = |owner| Record {
            version: RECORD_VERSION,
            sid: i32::try_from(pid).unwrap(),
            leader_start: Some((start.0, start.1)),
            owner,
        };
        // Recorded by a running server (this process): left alone.
        crate::atomic_state::commit(&path, &record(this_process())).unwrap();
        assert_eq!(
            recover_record(&path).unwrap(),
            crate::platform::RecordRecovery::Owned
        );
        // An older record naming no server, its session alive: left alone too.
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"sid":{pid},"leader_start":[{},{}]}}"#, start.0, start.1),
        )
        .unwrap();
        assert_eq!(
            recover_record(&path).unwrap(),
            crate::platform::RecordRecovery::Owned
        );
        assert!(child.try_wait().unwrap().is_none());
        // Recorded by a server that is gone (this pid, started at another time): swept.
        let (me, (seconds, fraction)) = this_process().unwrap();
        let gone = Some((me, (seconds.wrapping_add(1), fraction)));
        crate::atomic_state::commit(&path, &record(gone)).unwrap();
        assert_eq!(
            recover_record(&path).unwrap(),
            crate::platform::RecordRecovery::Swept
        );
        let status = child.wait().unwrap();
        assert!(!status.success());
        // With its session gone, an older record has nothing left to sweep.
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"sid":{pid},"leader_start":[{},{}]}}"#, start.0, start.1),
        )
        .unwrap();
        assert_eq!(
            recover_record(&path).unwrap(),
            crate::platform::RecordRecovery::Swept
        );
    }
}
