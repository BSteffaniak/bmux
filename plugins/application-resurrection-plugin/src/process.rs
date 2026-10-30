//! Bounded native process identity probes used for resurrection authorization.
//! Process names and command lines are deliberately not authorization inputs.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    pub pid: u32,
    start: (u64, u64),
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Process {
    pub identity: Identity,
    pub parent: u32,
    pub group: i64,
    pub session: i64,
    pub terminal: i64,
    pub foreground: i64,
}

pub(super) fn inspect(pid: u32) -> Result<Process, String> {
    inspect_native(pid).ok_or_else(|| format!("process identity unavailable for PID {pid}"))
}

pub(super) fn current(identity: Identity) -> bool {
    inspect_native(identity.pid).is_some_and(|process| process.identity == identity)
}

pub(super) fn executable(pid: u32) -> Result<std::path::PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|error| error.to_string())
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        let mut bytes = [0_u8; 4096];
        let pid = i32::try_from(pid).map_err(|error| error.to_string())?;
        // SAFETY: the bounded writable buffer is valid for its advertised size.
        let written = unsafe { libc::proc_pidpath(pid, bytes.as_mut_ptr().cast(), 4096) };
        if written <= 0 {
            return Err("shell executable unavailable".into());
        }
        let length = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("invalid shell executable path")?;
        Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
            &bytes[..length],
        )))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        Err("native executable inspection unavailable".into())
    }
}

pub(super) fn ancestor(peer: u32, ancestor: u32) -> Result<Process, String> {
    let mut observed = Vec::new();
    let mut pid = peer;
    for _ in 0..64 {
        let process = inspect(pid)?;
        observed.push(process.identity);
        if pid == ancestor {
            if observed.into_iter().all(current) {
                return Ok(process);
            }
            return Err("process ancestry changed during inspection".into());
        }
        if process.parent == 0 || process.parent == pid {
            break;
        }
        pid = process.parent;
    }
    Err("shell callback ancestry unavailable or exceeds limit".into())
}

pub(super) fn foreground_child(peer: u32, root: Identity, shell: bool) -> Result<bool, String> {
    let mut process = inspect(peer)?;
    let owner = inspect(root.pid)?;
    if shell {
        // Shells can spawn the child before tcsetpgrp completes. Wait only for
        // that job-control handoff, retaining the exact process incarnation.
        let identity = process.identity;
        for _ in 0..50 {
            if process.group == process.foreground {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
            process = inspect(peer)?;
            if process.identity != identity {
                return Ok(false);
            }
        }
    }
    if owner.identity != root {
        return Ok(false);
    }
    if !shell {
        return Ok(process.identity == root);
    }
    Ok(shell_member(process, owner) && current(process.identity) && current(root))
}

fn shell_member(process: Process, owner: Process) -> bool {
    (process.parent == owner.identity.pid || process.identity == owner.identity)
        && process.group > 0
        && process.group == process.foreground
        && process.session == owner.session
        && process.terminal != 0
        && process.terminal != -1
        && process.terminal == owner.terminal
}

#[cfg(target_os = "linux")]
fn inspect_native(pid: u32) -> Option<Process> {
    use std::io::Read as _;
    let mut stat = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(8193)
        .read_to_string(&mut stat)
        .ok()?;
    if stat.len() > 8192 {
        return None;
    }
    parse_linux(pid, &stat)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux(pid: u32, stat: &str) -> Option<Process> {
    let (prefix, rest) = stat.rsplit_once(')')?;
    if prefix.split_once('(')?.0.trim().parse::<u32>().ok()? != pid {
        return None;
    }
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    if matches!(state, "Z" | "X" | "x") {
        return None;
    }
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    let terminal = fields.next()?.parse().ok()?;
    let foreground = fields.next()?.parse().ok()?;
    // Fields 9..21 precede the process start time (field 22).
    let birth = fields.nth(13)?.parse().ok()?;
    Some(Process {
        identity: Identity {
            pid,
            start: (birth, 0),
        },
        parent,
        group,
        session,
        terminal,
        foreground,
    })
}

#[cfg(target_os = "macos")]
fn inspect_native(pid: u32) -> Option<Process> {
    let pid_signed = i32::try_from(pid).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the output buffer has precisely the advertised writable size.
    if unsafe {
        libc::proc_pidinfo(
            pid_signed,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    } != size
    {
        return None;
    }
    // SAFETY: proc_pidinfo initialized the complete structure above.
    let info = unsafe { info.assume_init() };
    // SAFETY: getsid reads process metadata and has no pointer arguments.
    let session = unsafe { libc::getsid(pid_signed) };
    if session < 0 || info.pbi_status == 5 {
        return None;
    }
    Some(Process {
        identity: Identity {
            pid,
            start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        },
        parent: info.pbi_ppid,
        group: i64::from(info.pbi_pgid),
        session: i64::from(session),
        terminal: if info.e_tdev == u32::MAX {
            -1
        } else {
            i64::from(info.e_tdev)
        },
        foreground: i64::from(info.e_tpgid),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn inspect_native(_pid: u32) -> Option<Process> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_identity_preserves_process_birth_and_job_control() {
        let stat = "42 (a name ) with (parentheses)) S 12 42 10 34818 42 0 1 2 3 4 5 6 7 8 9 10 11 12 98765 0";
        let process = parse_linux(42, stat).unwrap();
        assert_eq!(process.identity.start, (98765, 0));
        assert_eq!(process.parent, 12);
        assert_eq!(process.group, 42);
        assert_eq!(process.foreground, 42);
        assert_eq!(process.session, 10);
        assert_eq!(process.terminal, 34818);
        assert!(parse_linux(43, stat).is_none());
        assert!(parse_linux(42, &stat.replace(") S ", ") Z ")).is_none());
        assert!(parse_linux(42, "42 (truncated) S 12").is_none());
    }

    #[test]
    fn shell_membership_requires_foreground_same_terminal_and_session() {
        let owner = Process {
            identity: Identity {
                pid: 10,
                start: (1, 0),
            },
            parent: 1,
            group: 10,
            session: 10,
            terminal: 34818,
            foreground: 20,
        };
        let child = Process {
            identity: Identity {
                pid: 20,
                start: (2, 0),
            },
            parent: 10,
            group: 20,
            foreground: 20,
            ..owner
        };
        assert!(shell_member(child, owner));
        // An exec preserves the shell's process identity, but still must own
        // the foreground terminal. Merely inheriting the endpoint is insufficient.
        let replaced = Process {
            foreground: 10,
            ..owner
        };
        assert!(shell_member(replaced, replaced));
        for rejected in [
            Process {
                parent: 11,
                ..child
            },
            Process {
                foreground: 30,
                ..child
            },
            Process {
                group: 0,
                foreground: 0,
                ..child
            },
            Process {
                session: 30,
                ..child
            },
            Process {
                terminal: 34819,
                ..child
            },
            Process {
                terminal: 0,
                ..child
            },
            Process {
                terminal: -1,
                ..child
            },
        ] {
            assert!(!shell_member(rejected, owner), "accepted {rejected:?}");
        }
    }

    #[test]
    fn native_identity_is_current_and_explicit_launch_matches_exactly() {
        let pid = std::process::id();
        let process = inspect(pid).unwrap();
        assert!(current(process.identity));
        assert!(foreground_child(pid, process.identity, false).unwrap());
        let stale = Identity {
            start: (0, 0),
            ..process.identity
        };
        assert!(!current(stale));
        assert!(!foreground_child(pid, stale, false).unwrap());
    }
}
