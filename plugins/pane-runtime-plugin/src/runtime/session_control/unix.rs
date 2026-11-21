//! Unix session authority: the leader's session ID is the pane's identity.
use crate::runtime::session_sweep;
use serde::{Deserialize, Serialize};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use switchy_thread::sync::Mutex;

/// Nothing has to exist before a Unix spawn: `setsid` happens in the child.
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
    /// Spawn the pane's program as the leader of a new session on `slave`.
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
    sid: i32,
    start: Option<crate::runtime::ProcessStartIdentity>,
    guard: Mutex<Option<crate::runtime::crash_guard::Registration>>,
}

const RECORD_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    version: u32,
    sid: i32,
    /// The leader's start identity; `None` when the platform reports none.
    leader_start: Option<(u64, u64)>,
}

impl Session {
    pub(super) fn adopt(
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
        let start = crate::runtime::process_start_identity_for_pid(leader);
        let guard = crate::runtime::crash_guard::register(sid, Some(master.as_raw_fd()));
        Ok(Self {
            sid,
            start,
            guard: Mutex::new(guard),
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
                sid: self.sid,
                leader_start: self.start.map(|start| (start.0, start.1)),
            },
        )?;
        Ok(Some(path))
    }

    #[expect(clippy::unused_self, reason = "same signature as the Windows backend")]
    pub(super) fn terminate_leader(&self, leader: u32) -> std::io::Result<bool> {
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

    pub(super) fn sweep(&self) -> std::io::Result<()> {
        session_sweep::sweep(self.sid)
    }

    pub(super) fn occupied(&self) -> std::io::Result<bool> {
        session_sweep::occupied(self.sid)
    }

    #[expect(
        clippy::unused_self,
        reason = "Windows needs the session to release its job"
    )]
    pub(super) const fn release_survivors(&self) {}

    pub(super) fn release(&self) {
        if let Ok(mut guard) = self.guard.lock() {
            guard.take();
        }
    }
}

/// Sweep the session one crash record describes, if it is still ours.
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
        .map(|(seconds, fraction)| crate::runtime::ProcessStartIdentity(seconds, fraction));
    if !still_ours(leader, recorded) {
        return Ok(());
    }
    session_sweep::sweep(record.sid)?;
    Ok(())
}

/// Whether the recorded session still belongs to the pane that wrote it.
///
/// A live process whose pid equals the session ID must be the recorded leader.
/// Otherwise the number was reused by an unrelated session leader. When the
/// leader is gone, its session ID stays reserved while any member lives, so
/// remaining members are ours; members that started before the recorded leader
/// cannot be, and their presence means the record is stale.
fn still_ours(leader: u32, recorded: Option<crate::runtime::ProcessStartIdentity>) -> bool {
    let Some(recorded) = recorded else {
        // Without a start identity there is no way to rule out reuse.
        return false;
    };
    if let Some(current) = crate::runtime::process_start_identity_for_pid(leader) {
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
            .and_then(crate::runtime::process_start_identity_for_pid)
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
        let mine = crate::runtime::process_start_identity_for_pid(me).unwrap();
        let other = crate::runtime::ProcessStartIdentity(mine.0.wrapping_add(1), mine.1);
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
        let start = crate::runtime::process_start_identity_for_pid(pid).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("record.json");
        crate::atomic_state::commit(
            &path,
            &Record {
                version: RECORD_VERSION,
                sid: i32::try_from(pid).unwrap(),
                leader_start: Some((start.0, start.1)),
            },
        )
        .unwrap();
        recover_record(&path).unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
