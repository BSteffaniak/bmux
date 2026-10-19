//! Deadline-bound subprocess IO for private cleanup probes. Never signals a
//! discovered PID: cancellation owns the still-unreaped direct Child.
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

struct ProbeChild(Option<Child>);
impl Drop for ProbeChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            // A kernel-stuck child must not extend the caller's deadline. Keep
            // ownership in a reaper, rather than leak a dropped std Child.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

pub(super) fn capture(
    command: &mut Command,
    deadline: Instant,
    limit: usize,
) -> io::Result<Vec<u8>> {
    check_deadline(deadline)?;
    let mut child = ProbeChild(Some(command.spawn()?));
    let mut stdout = child
        .0
        .as_mut()
        .expect("owned probe child")
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cleanup probe stdout unavailable"))?;
    // SAFETY: valid owned pipe descriptor; preserve flags and make reads bounded.
    unsafe {
        let flags = libc::fcntl(stdout.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    let (status, bytes) = collect(&mut child, Some(&mut stdout), deadline, limit)?;
    if !status.success() {
        return Err(io::Error::other("session cleanup process snapshot failed"));
    }
    Ok(bytes)
}

pub(super) fn status(command: &mut Command, deadline: Instant) -> io::Result<ExitStatus> {
    check_deadline(deadline)?;
    let mut child = ProbeChild(Some(command.spawn()?));
    Ok(collect(&mut child, None, deadline, 0)?.0)
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "session cleanup probe deadline exceeded",
        ));
    }
    Ok(())
}

fn collect(
    child: &mut ProbeChild,
    mut reader: Option<&mut dyn Read>,
    deadline: Instant,
    limit: usize,
) -> io::Result<(ExitStatus, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut outcome = None;
    loop {
        check_deadline(deadline)?;
        if let Some(pipe) = reader.as_mut() {
            let mut chunk = [0; 8192];
            match pipe.read(&mut chunk) {
                Ok(0) => reader = None,
                Ok(count) => {
                    if count > limit.saturating_sub(bytes.len()) {
                        return Err(io::Error::other(
                            "session cleanup process snapshot exceeds limit",
                        ));
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                    // Recheck the deadline even if a producer never stops writing.
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        }
        if outcome.is_none() {
            outcome = child.0.as_mut().expect("owned probe child").try_wait()?;
            if outcome.is_some() {
                // try_wait reaped it. Never retain a signaling guard for a reaped
                // PID while a descendant might still hold the output pipe open.
                child.0.take();
            }
        }
        if reader.is_none()
            && let Some(status) = outcome
        {
            return Ok((status, bytes));
        }
        std::thread::sleep(
            Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn command(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }
    fn reaped(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: inspect the private fixture without stealing the reaper's
            // wait. WNOWAIT preserves its Child ownership until ECHILD proves
            // the asynchronous reaper completed.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == -1 {
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                return;
            }
            assert_eq!(
                result, 0,
                "probe cancellation failed to retain reaping ownership"
            );
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn stalled_output_and_stalled_exit_are_deadline_bound_and_reaped() {
        for script in ["exec /bin/sleep 60", "exec 1>&-; exec /bin/sleep 60"] {
            let start = Instant::now();
            let mut child = ProbeChild(Some(command(script).spawn().unwrap()));
            let pid = child.0.as_ref().unwrap().id();
            let mut stdout = child.0.as_mut().unwrap().stdout.take().unwrap();
            // SAFETY: fixture pipe only; nonblocking matches production capture.
            unsafe {
                assert_eq!(
                    libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK),
                    0
                );
            }
            let error = collect(
                &mut child,
                Some(&mut stdout),
                start + Duration::from_millis(30),
                1024,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            drop(child);
            assert!(start.elapsed() < Duration::from_secs(1));
            reaped(pid);
        }
    }
    #[test]
    fn read_failure_cancels_and_reaps_probe() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected read failure"))
            }
        }
        let mut child = ProbeChild(Some(command("exec /bin/sleep 60").spawn().unwrap()));
        let pid = child.0.as_ref().unwrap().id();
        let error = collect(
            &mut child,
            Some(&mut Broken),
            Instant::now() + Duration::from_secs(1),
            1024,
        )
        .unwrap_err();
        assert!(error.to_string().contains("injected read failure"));
        drop(child);
        reaped(pid);
    }
    #[test]
    fn failed_and_oversized_enumeration_are_errors() {
        let deadline = Instant::now() + Duration::from_secs(1);
        assert!(
            capture(&mut command("exit 7"), deadline, 1024)
                .unwrap_err()
                .to_string()
                .contains("snapshot failed")
        );
        assert!(
            capture(&mut command("printf 12345"), deadline, 4)
                .unwrap_err()
                .to_string()
                .contains("exceeds limit")
        );
    }
    #[test]
    fn join_wait_is_deadline_bound_and_preserves_failure_status() {
        let start = Instant::now();
        assert_eq!(
            status(
                &mut command("exec /bin/sleep 60"),
                start + Duration::from_millis(30)
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            status(
                &mut command("exit 7"),
                Instant::now() + Duration::from_secs(1)
            )
            .unwrap()
            .code(),
            Some(7)
        );
    }
}
