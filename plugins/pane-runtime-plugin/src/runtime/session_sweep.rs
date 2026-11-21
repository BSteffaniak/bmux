//! In-server cleanup of one pane's terminal session.
//!
//! Every pane process starts as the leader of its own session, and its pid is
//! therefore the session ID. Jobs started by the shell stay in that session
//! unless they call `setsid()` themselves; such processes are out of scope, as
//! they were for the former out-of-process supervisor.
//!
//! Neither kernel will hand out a pid while it is still in use as a session or
//! process-group ID, so "every live process whose session is `sid`" is a stable
//! identity, not a racy snapshot of numeric pids. The kill step re-checks
//! membership right before signalling:
//!
//! - Linux opens a pidfd for each member, re-checks its session through the
//!   pinned pid and signals the pidfd, so a reused pid is never signalled.
//! - Elsewhere the kill targets whole process groups after re-checking that a
//!   member of the group is still in the session. The window that remains (the
//!   whole group dies and its number is reused in between) is accepted and
//!   documented in `docs/development/in-server-execution-plan.md`.
//!
//! Verification only trusts rescans: a signal delivered is not proof of exit.
use std::collections::BTreeSet;
use std::io;
use std::time::Duration;
use switchy_time::Instant;

/// One live, non-zombie session member.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Member {
    pub(super) pid: i32,
    pub(super) group: i32,
}

/// The default whole-sweep budget: discovery, kill passes and verification.
pub(super) const SWEEP_BUDGET: Duration = Duration::from_secs(2);

/// Kill every live member of `sid` and verify that none remain.
///
/// # Errors
/// Discovery failures, signal failures other than "already gone", and members
/// that are still alive once verification gives up.
pub(super) fn sweep(sid: i32) -> io::Result<()> {
    if sid <= 1 {
        return Err(io::Error::other("refusing to sweep an invalid session"));
    }
    let deadline = Instant::now() + SWEEP_BUDGET;
    sweep_with(
        || members(sid, deadline),
        |batch| kill(sid, batch),
        deadline,
    )
}

/// The pids of every live member of `sid`.
///
/// # Errors
/// Discovery failures.
pub(super) fn member_pids(sid: i32) -> io::Result<Vec<i32>> {
    Ok(members(sid, Instant::now() + SWEEP_BUDGET)?
        .into_iter()
        .map(|member| member.pid)
        .collect())
}

/// Whether `sid` still has any live member.
///
/// # Errors
/// Discovery failures.
pub(super) fn occupied(sid: i32) -> io::Result<bool> {
    Ok(!members(sid, Instant::now() + SWEEP_BUDGET)?.is_empty())
}

fn sweep_with(
    mut snapshot: impl FnMut() -> io::Result<BTreeSet<Member>>,
    mut kill_batch: impl FnMut(&BTreeSet<Member>) -> io::Result<()>,
    deadline: Instant,
) -> io::Result<()> {
    // Bounded rescans catch ordinary fork races, not adversarial forking.
    for _ in 0..3 {
        let batch = snapshot()?;
        if batch.is_empty() {
            break;
        }
        kill_batch(&batch)?;
    }
    // Delivery is asynchronous. Let ordinary members finish exiting, but never
    // report success while anything in the session is still alive.
    let verify_until = (Instant::now() + Duration::from_millis(500)).min(deadline);
    loop {
        let residual = snapshot()?;
        if residual.is_empty() {
            return Ok(());
        }
        if Instant::now() >= verify_until {
            let pids: Vec<i32> = residual.iter().map(|member| member.pid).collect();
            return Err(io::Error::other(format!(
                "live terminal-session members remain after cleanup: {pids:?}"
            )));
        }
        switchy_thread::thread::sleep(Duration::from_millis(10));
    }
}

fn session_of(pid: i32) -> io::Result<Option<i32>> {
    // SAFETY: getsid has no memory-safety preconditions.
    let session = unsafe { libc::getsid(pid) };
    if session >= 0 {
        return Ok(Some(session));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    Err(error)
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "terminal-session scan deadline exceeded",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn members(sid: i32, deadline: Instant) -> io::Result<BTreeSet<Member>> {
    use std::io::Read;
    let mut result = BTreeSet::new();
    for entry in std::fs::read_dir("/proc")? {
        check_deadline(deadline)?;
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // Cheap pre-filter before opening every stat file on busy hosts.
        if session_of(pid)? != Some(sid) {
            continue;
        }
        let mut bytes = Vec::new();
        let read = std::fs::File::open(entry.path().join("stat"))
            .and_then(|file| file.take(8193).read_to_end(&mut bytes));
        match read {
            // Exiting processes can disappear between enumeration and read.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(error) => return Err(error),
            Ok(_) => {}
        }
        if bytes.len() > 8192 {
            return Err(io::Error::other("proc stat exceeds limit"));
        }
        let stat = parse_stat(&bytes)?;
        if !stat.exited && stat.session == sid && stat.group > 0 {
            result.insert(Member {
                pid,
                group: stat.group,
            });
        }
    }
    Ok(result)
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
struct Stat {
    group: i32,
    session: i32,
    exited: bool,
}

#[cfg(any(target_os = "linux", test))]
fn parse_stat(bytes: &[u8]) -> io::Result<Stat> {
    // comm is parenthesized and can contain spaces, ')' and non-UTF-8 bytes.
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| io::Error::other("missing proc stat command"))?;
    let tail = std::str::from_utf8(&bytes[end + 1..]).map_err(io::Error::other)?;
    let mut fields = tail.split_whitespace();
    let mut next = |name: &str| {
        fields
            .next()
            .ok_or_else(|| io::Error::other(format!("missing proc {name}")))
    };
    let state = next("state")?;
    let _parent = next("parent")?;
    let group = next("group")?.parse().map_err(io::Error::other)?;
    let session = next("session")?.parse().map_err(io::Error::other)?;
    Ok(Stat {
        group,
        session,
        exited: matches!(state, "Z" | "X" | "x"),
    })
}

#[cfg(target_os = "macos")]
fn members(sid: i32, deadline: Instant) -> io::Result<BTreeSet<Member>> {
    /// `pbi_status` of a zombie (`<sys/proc.h>`).
    const SZOMB: u32 = 5;
    let mut pids = vec![0_i32; 4096];
    let count = loop {
        check_deadline(deadline)?;
        let bytes =
            i32::try_from(std::mem::size_of_val(pids.as_slice())).map_err(io::Error::other)?;
        // SAFETY: the buffer is writable for exactly `bytes` bytes.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let count = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
        if count < pids.len() {
            break count;
        }
        // A full buffer may have truncated the list; retry with more room.
        pids.resize(pids.len() * 2, 0);
    };
    let mut result = BTreeSet::new();
    for &pid in &pids[..count] {
        check_deadline(deadline)?;
        if pid <= 0 || session_of(pid)? != Some(sid) {
            continue;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size =
            i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).map_err(io::Error::other)?;
        // SAFETY: the writable buffer has exactly the advertised record size.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            // Exited between enumeration and inspection.
            continue;
        }
        // SAFETY: proc_pidinfo returned the complete initialized record.
        let info = unsafe { info.assume_init() };
        let group = i32::try_from(info.pbi_pgid).map_err(io::Error::other)?;
        if info.pbi_status != SZOMB && group > 0 {
            result.insert(Member { pid, group });
        }
    }
    Ok(result)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn members(_sid: i32, _deadline: Instant) -> io::Result<BTreeSet<Member>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "terminal-session discovery is unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn kill(sid: i32, batch: &BTreeSet<Member>) -> io::Result<()> {
    for member in batch {
        // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, member.pid, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => continue,
                // Kernels before 5.3: fall back to the checked group kill.
                Some(libc::ENOSYS) => {
                    kill_group_checked(sid, *member)?;
                    continue;
                }
                _ => return Err(error),
            }
        }
        let fd = i32::try_from(raw).map_err(io::Error::other)?;
        // SAFETY: pidfd_open returned a fresh descriptor that nothing else owns.
        let pidfd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        // The pidfd now pins this process. Re-check that the pid still names a
        // member of the session: if the scanned process had exited and the pid
        // was reused before the open, the pidfd refers to the newcomer, which
        // fails this check and is left alone.
        if session_of(member.pid)? != Some(sid) {
            continue;
        }
        // SAFETY: a live pidfd, a valid signal, no siginfo, no flags.
        let sent = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                std::os::fd::AsRawFd::as_raw_fd(&pidfd),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if sent < 0 {
            let error = io::Error::last_os_error();
            if !gone_or_unsignalable(&error) {
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn kill(sid: i32, batch: &BTreeSet<Member>) -> io::Result<()> {
    let mut signalled = BTreeSet::new();
    for member in batch {
        if signalled.insert(member.group) {
            kill_group_checked(sid, *member)?;
        }
    }
    Ok(())
}

/// Signal the member's whole group after re-checking that the member is still
/// in the session and still in that group.
fn kill_group_checked(sid: i32, member: Member) -> io::Result<()> {
    if session_of(member.pid)? != Some(sid) {
        return Ok(());
    }
    // SAFETY: getpgid has no memory-safety preconditions.
    if unsafe { libc::getpgid(member.pid) } != member.group {
        return Ok(());
    }
    // SAFETY: a positive group verified as part of the session just above.
    if unsafe { libc::kill(-member.group, libc::SIGKILL) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if gone_or_unsignalable(&error) {
        return Ok(());
    }
    Err(error)
}

/// `ESRCH`: the target is already gone. `EPERM`: nothing in it may be signalled,
/// such as a group left with only an exiting or privileged process. Neither is a
/// sweep failure by itself: the rescan reports anything that is really still
/// alive.
fn gone_or_unsignalable(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EPERM))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    fn member(pid: i32) -> Member {
        Member { pid, group: pid }
    }

    #[test]
    fn residual_members_fail_the_sweep() {
        let deadline = Instant::now() + SWEEP_BUDGET;
        let error =
            sweep_with(|| Ok(BTreeSet::from([member(42)])), |_| Ok(()), deadline).unwrap_err();
        assert!(error.to_string().contains("members remain"));
    }

    #[test]
    fn kill_failure_is_not_a_successful_sweep() {
        let deadline = Instant::now() + SWEEP_BUDGET;
        let error = sweep_with(
            || Ok(BTreeSet::from([member(42)])),
            |_| Err(io::Error::other("injected kill failure")),
            deadline,
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected kill failure"));
    }

    #[test]
    fn a_final_scan_is_required_after_the_last_signal() {
        let mut scans = 0;
        let deadline = Instant::now() + SWEEP_BUDGET;
        let error = sweep_with(
            || {
                scans += 1;
                Ok(if scans == 1 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([member(99)])
                })
            },
            |_| unreachable!("an empty first scan has nothing to kill"),
            deadline,
        )
        .unwrap_err();
        assert!(error.to_string().contains("members remain"));
    }

    #[test]
    fn missing_and_unsignalable_targets_are_left_to_the_rescan() {
        for code in [libc::ESRCH, libc::EPERM] {
            assert!(gone_or_unsignalable(&io::Error::from_raw_os_error(code)));
        }
        assert!(!gone_or_unsignalable(&io::Error::from_raw_os_error(
            libc::EINVAL
        )));
    }

    #[test]
    fn invalid_sessions_are_refused() {
        assert!(sweep(0).is_err());
        assert!(sweep(1).is_err());
    }

    #[test]
    fn command_names_cannot_confuse_stat_fields() {
        assert_eq!(
            parse_stat(b"12 (odd ) name\xff) S 1 42 7 0\n").unwrap(),
            Stat {
                group: 42,
                session: 7,
                exited: false
            }
        );
        assert!(parse_stat(b"12 (zombie) Z 1 42 7").unwrap().exited);
        assert!(parse_stat(b"12 (truncated) S 1").is_err());
        assert!(parse_stat(b"12 (invalid) S 1 nope 7").is_err());
    }

    /// A real session whose leader ignores SIGHUP and SIGTERM, with a
    /// HUP-ignoring background job in a separate process group.
    #[test]
    fn sweep_kills_every_group_in_a_real_session() {
        let mut leader = std::process::Command::new("/bin/sh");
        leader
            .args([
                "-c",
                "trap '' HUP TERM; set -m; (trap '' HUP TERM; sleep 60) & sleep 60",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            leader.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = leader.spawn().unwrap();
        let sid = i32::try_from(child.id()).unwrap();
        let ready = Instant::now() + Duration::from_secs(5);
        while members(sid, Instant::now() + SWEEP_BUDGET).unwrap().len() < 3 {
            assert!(Instant::now() < ready, "session never populated");
            switchy_thread::thread::sleep(Duration::from_millis(10));
        }
        let groups: BTreeSet<i32> = members(sid, Instant::now() + SWEEP_BUDGET)
            .unwrap()
            .iter()
            .map(|member| member.group)
            .collect();
        assert!(
            groups.len() >= 2,
            "the job runs in its own group: {groups:?}"
        );
        // The leader is our child, so it stays a zombie (excluded) until reaped.
        sweep(sid).unwrap();
        assert!(!occupied(sid).unwrap());
        let _ = child.wait();
    }

    #[test]
    fn processes_that_escape_with_setsid_are_left_alone() {
        let mut outsider = std::process::Command::new("sleep");
        outsider.arg("60");
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            outsider.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut outsider = outsider.spawn().unwrap();
        let outsider_pid = i32::try_from(outsider.id()).unwrap();
        // Sweeping our own (test runner) session would be destructive, so sweep
        // an unrelated empty session and check the outsider survives.
        let mut empty = std::process::Command::new("/bin/sh");
        empty.args(["-c", "exit 0"]);
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            empty.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut empty = empty.spawn().unwrap();
        let empty_sid = i32::try_from(empty.id()).unwrap();
        let _ = empty.wait();
        sweep(empty_sid).unwrap();
        assert_eq!(session_of(outsider_pid).unwrap(), Some(outsider_pid));
        let _ = outsider.kill();
        let _ = outsider.wait();
    }
}
