//! Same-session crash cleanup, not containment for setsid() escapees.
//!
//! A fresh helper joins a candidate group before kill(0): setpgid enforces
//! same-session membership and the helper pins group identity. Snapshots never
//! authorize numeric PID kills. Final verification checks live residual members;
//! the joining helper's own SIGKILL alone does not prove it killed its peers.
use std::collections::BTreeSet;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "cleanup_probe.rs"]
mod probe;

#[cfg(target_os = "linux")]
#[path = "linux_process_groups.rs"]
mod linux_process_groups;
#[cfg(target_os = "linux")]
use linux_process_groups::groups;

// These are administrator-managed system locations, including NixOS's system
// profile. They carry the same trusted-system authority assumption as /bin/ps;
// never search the caller's PATH or fall back after an execution failure.
#[cfg(not(target_os = "linux"))]
fn ps_program() -> io::Result<&'static std::path::Path> {
    select_ps_program(
        &["/bin/ps", "/usr/bin/ps", "/run/current-system/sw/bin/ps"],
        |path| {
            use std::os::unix::fs::PermissionsExt;
            let metadata = path.metadata()?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cleanup ps is not an executable file",
                ));
            }
            Ok(())
        },
    )
}

#[cfg(any(not(target_os = "linux"), test))]
fn select_ps_program<'a>(
    candidates: &[&'a str],
    mut check: impl FnMut(&std::path::Path) -> io::Result<()>,
) -> io::Result<&'a std::path::Path> {
    for &candidate in candidates {
        let path = std::path::Path::new(candidate);
        match check(path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "cleanup ps unavailable in trusted system locations",
    ))
}

#[cfg(not(target_os = "linux"))]
fn groups(
    session: i32,
    own_group: i32,
    protected_group: Option<u32>,
    deadline: Instant,
) -> io::Result<BTreeSet<i32>> {
    let bytes = probe::capture(
        Command::new(ps_program()?)
            .args(["-axo", "pid=,pgid=,stat="])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        deadline,
        1_048_576,
    )?;
    let text = String::from_utf8(bytes).map_err(io::Error::other)?;
    let mut result = BTreeSet::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let pid: i32 = fields
            .next()
            .ok_or_else(|| io::Error::other("missing ps PID"))?
            .parse()
            .map_err(io::Error::other)?;
        let group: i32 = fields
            .next()
            .ok_or_else(|| io::Error::other("missing ps group"))?
            .parse()
            .map_err(io::Error::other)?;
        let status = fields
            .next()
            .ok_or_else(|| io::Error::other("missing ps status"))?;
        // Zombie processes have already exited; their parent owns reaping.
        // getsid is only candidate discovery. A joining helper revalidates it.
        if !status.starts_with('Z')
            && pid > 0
            && unsafe { libc::getsid(pid) } == session
            && group > 0
            && group != session
            && group != own_group
            && u32::try_from(group).ok() != protected_group
        {
            result.insert(group);
        }
    }
    Ok(result)
}

pub(super) fn sweep(protected_group: Option<u32>) -> io::Result<()> {
    let session = unsafe { libc::getsid(0) };
    let own_group = unsafe { libc::getpgrp() };
    if session < 0 {
        return Err(io::Error::last_os_error());
    }
    // Every enumeration and join shares this deadline; repeated probes cannot
    // multiply the retirement budget. Spawn/syscall scheduling itself remains
    // subject to the OS; no blocking pipe reads or child waits occur here.
    let deadline = Instant::now() + Duration::from_secs(2);
    sweep_with(
        || groups(session, own_group, protected_group, deadline),
        |group| {
            let status = probe::status(
                Command::new(std::env::current_exe()?)
                    .arg("--join-and-kill-group")
                    .arg(group.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null()),
                deadline,
            )?;
            // Exit 2 means membership was refused (gone or unrelated). A killed
            // helper is not proof of peer termination: final scans are mandatory.
            if status.code() == Some(2) || status.signal() == Some(libc::SIGKILL) {
                Ok(())
            } else {
                Err(io::Error::other("session group cleanup helper failed"))
            }
        },
    )
}

fn sweep_with(
    mut snapshot: impl FnMut() -> io::Result<BTreeSet<i32>>,
    mut kill_group: impl FnMut(i32) -> io::Result<()>,
) -> io::Result<()> {
    // Bounded rescans catch ordinary fork races, not adversarial forking. A
    // failed helper remains a cleanup error even if a later scan looks empty.
    for _ in 0..3 {
        for group in snapshot()? {
            kill_group(group)?;
        }
    }
    // Signal delivery is asynchronous. Allow ordinary exiting members to retire,
    // but do not return success for unsignalable/stuck members or helper-only death.
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let residual = snapshot()?;
        if residual.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "live terminal-session groups remain after cleanup: {residual:?}"
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn join_and_kill(group: &str) -> io::Result<()> {
    let group: i32 = group.parse().map_err(io::Error::other)?;
    if group <= 0 || group == unsafe { libc::getsid(0) } {
        return Err(io::Error::other("invalid cleanup group"));
    }
    // SAFETY: joining pins identity and the kernel rejects other sessions.
    unsafe {
        if libc::setpgid(0, group) != 0 {
            std::process::exit(2);
        }
        libc::kill(0, libc::SIGKILL);
    }
    Err(io::Error::other("session group kill returned"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_ps_selection_is_ordered_and_missing_is_fail_closed() {
        let candidates = ["/bin/ps", "/usr/bin/ps", "/run/current-system/sw/bin/ps"];
        for available in 0..candidates.len() {
            let mut visited = Vec::new();
            let selected = select_ps_program(&candidates, |path| {
                visited.push(path.to_owned());
                if path == std::path::Path::new(candidates[available]) {
                    Ok(())
                } else {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                }
            })
            .unwrap();
            assert_eq!(selected, std::path::Path::new(candidates[available]));
            assert_eq!(visited.len(), available + 1);
        }
        let error = select_ps_program(&candidates, |_| {
            Err(io::Error::from(io::ErrorKind::NotFound))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("trusted system locations"));
    }

    #[test]
    fn trusted_ps_selection_does_not_mask_access_or_metadata_failures() {
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
            let mut checks = 0;
            let error = select_ps_program(&["/bin/ps", "/usr/bin/ps"], |_| {
                checks += 1;
                Err(io::Error::from(kind))
            })
            .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(checks, 1, "only absence permits the next system location");
        }
    }

    #[test]
    fn residual_members_are_not_hidden_by_successful_join_helper() {
        let error = sweep_with(|| Ok(BTreeSet::from([42])), |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("groups remain"));
    }
    #[test]
    fn helper_failure_is_not_a_successful_sweep() {
        let error = sweep_with(
            || Ok(BTreeSet::from([42])),
            |_| Err(io::Error::other("injected helper failure")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected helper failure"));
    }
    #[test]
    fn final_scan_is_required_after_last_signal() {
        let mut scans = 0;
        let error = sweep_with(
            || {
                scans += 1;
                Ok(if scans <= 3 {
                    BTreeSet::new()
                } else {
                    BTreeSet::from([99])
                })
            },
            |_| unreachable!(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("groups remain"));
    }
}
