use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(any(
    all(test, any(target_os = "linux", target_os = "macos")),
    all(unix, not(any(target_os = "linux", target_os = "macos")))
))]
use std::process::Command as ProcessCommand;

use super::{
    ConnectionContext, SERVER_POLL_INTERVAL, SERVER_STATUS_TIMEOUT, connect_raw_with_context,
    remove_server_runtime_metadata_file,
};

pub(super) async fn server_is_running(connection_context: ConnectionContext<'_>) -> Result<bool> {
    probe_server_running(connection_context).await
}

pub(super) async fn probe_server_running(
    connection_context: ConnectionContext<'_>,
) -> Result<bool> {
    Ok(fetch_server_status(connection_context)
        .await?
        .is_some_and(|status| status.running))
}

pub(super) async fn fetch_server_status(
    connection_context: ConnectionContext<'_>,
) -> Result<Option<bmux_client::ServerStatusInfo>> {
    let connect = tokio::time::timeout(
        SERVER_STATUS_TIMEOUT,
        connect_raw_with_context("bmux-cli-status", connection_context),
    )
    .await;

    match connect {
        Ok(Ok(mut client)) => {
            match tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await {
                Ok(Ok(status)) => Ok(Some(status)),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "server status request failed");
                    Ok(None)
                }
                Err(error) => {
                    tracing::debug!(%error, "server status request timed out");
                    Ok(None)
                }
            }
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "server status connection failed");
            Ok(None)
        }
        Err(error) => {
            tracing::debug!(%error, "server status connection timed out");
            Ok(None)
        }
    }
}

pub(super) async fn wait_for_server_running(
    timeout: Duration,
    connection_context: ConnectionContext<'_>,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let connect = tokio::time::timeout(
            SERVER_STATUS_TIMEOUT,
            connect_raw_with_context("bmux-cli-start-wait", connection_context),
        )
        .await;
        if let Ok(Ok(mut client)) = connect
            && let Ok(Ok(status)) =
                tokio::time::timeout(SERVER_STATUS_TIMEOUT, client.server_status()).await
            && status.running
        {
            return Ok(true);
        }
        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }
    Ok(false)
}

/// Remote endpoint disappearance is evidence only of transport closure, not
/// process exit or successful cleanup. The caller must report that limitation.
pub(super) async fn wait_until_server_stopped(
    deadline: tokio::time::Instant,
    target: &crate::connection::StopTarget,
) -> Result<bool> {
    loop {
        match tokio::time::timeout_at(deadline, target.connect("bmux-cli-stop-check")).await {
            Ok(Err(_)) => return Ok(true),
            Err(_) => return Ok(false),
            Ok(Ok(_)) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + SERVER_POLL_INTERVAL))
            .await;
    }
}

/// Retain an OS process observation, not a numeric-PID polling loop. pidfds,
/// kqueue registrations and Windows handles survive numeric PID reuse.
/// No shutdown path sends a signal merely because IPC is unavailable.
pub(super) struct ProcessObservation {
    #[cfg(target_os = "linux")]
    fd: Option<std::os::fd::OwnedFd>,
    #[cfg(target_os = "macos")]
    queue: Option<std::os::fd::OwnedFd>,
    #[cfg(target_os = "macos")]
    exit_seen: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    handle: Option<std::os::windows::io::OwnedHandle>,
}

impl ProcessObservation {
    pub(super) fn open(pid: u32) -> Result<Self> {
        anyhow::ensure!(pid > 0, "invalid server PID");
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            // SAFETY: pidfd_open accepts an integer PID and zero flags; it does
            // not dereference any pointers. The returned descriptor is owned.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            if fd < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(Self { fd: None });
                }
                return Err(error).context("cannot safely observe server identity with pidfd");
            }
            let fd = i32::try_from(fd).context("invalid pidfd descriptor")?;
            // SAFETY: a successful pidfd_open returns a new owned descriptor.
            Ok(Self {
                fd: Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }),
            })
        }
        #[cfg(target_os = "macos")]
        {
            Self::open_macos(pid)
        }
        #[cfg(windows)]
        {
            Self::open_windows(pid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            anyhow::bail!(
                "identity-safe local shutdown observation is unavailable on this platform; no signal sent"
            )
        }
    }

    pub(super) fn exited(&self) -> Result<bool> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let Some(fd) = &self.fd else {
                return Ok(true);
            };
            let mut pollfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pollfd references one initialized descriptor for this
            // call; timeout zero never blocks. pidfd readiness includes zombies.
            let result = unsafe { libc::poll(&raw mut pollfd, 1, 0) };
            if result < 0 {
                return Err(std::io::Error::last_os_error()).context("failed polling server pidfd");
            }
            anyhow::ensure!(pollfd.revents & libc::POLLNVAL == 0, "invalid server pidfd");
            Ok(pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0)
        }
        #[cfg(target_os = "macos")]
        {
            self.exited_macos()
        }
        #[cfg(windows)]
        {
            self.exited_windows()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            anyhow::bail!(
                "identity-safe local shutdown observation is unavailable on this platform"
            )
        }
    }

    #[cfg(target_os = "macos")]
    fn open_macos(pid: u32) -> Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        // SAFETY: kqueue takes no arguments and returns a new owned descriptor.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(std::io::Error::last_os_error()).context("failed creating process kqueue");
        }
        // SAFETY: successful kqueue returned a fresh owned descriptor.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        let change = libc::kevent {
            ident: usize::try_from(pid).context("invalid server PID")?,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ENABLE,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: change is initialized; there is one change, no output buffer,
        // and no wait when no output events are requested. Registration pins
        // this process rather than observing a future reuse of its PID.
        let result = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(Self {
                    queue: None,
                    exit_seen: std::sync::atomic::AtomicBool::new(true),
                });
            }
            return Err(error).context("failed registering server process exit observation");
        }
        Ok(Self {
            queue: Some(queue),
            exit_seen: std::sync::atomic::AtomicBool::new(false),
        })
    }

    #[cfg(target_os = "macos")]
    fn exited_macos(&self) -> Result<bool> {
        use std::os::fd::AsRawFd;
        use std::sync::atomic::Ordering;
        if self.exit_seen.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let Some(queue) = &self.queue else {
            return Ok(true);
        };
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: event has space for one kevent, no changes are submitted,
        // and the initialized zero timeout makes this call nonblocking.
        let result = unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                std::ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                &raw const timeout,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed polling server process kqueue");
        }
        if result == 0 {
            return Ok(false);
        }
        // SAFETY: kevent returned one initialized event.
        let event = unsafe { event.assume_init() };
        anyhow::ensure!(
            event.flags & libc::EV_ERROR == 0,
            "server process kqueue returned an error"
        );
        let exited = event.filter == libc::EVFILT_PROC && event.fflags & libc::NOTE_EXIT != 0;
        if exited {
            self.exit_seen.store(true, Ordering::Relaxed);
        }
        Ok(exited)
    }

    #[cfg(windows)]
    fn open_windows(pid: u32) -> Result<Self> {
        use std::os::windows::io::{FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, GetLastError};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        // SAFETY: request only wait access, no inheritance, for a positive PID.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // SAFETY: immediately read the calling thread's last OS error.
            let error = unsafe { GetLastError() };
            if error == ERROR_INVALID_PARAMETER {
                return Ok(Self { handle: None });
            }
            return Err(std::io::Error::from_raw_os_error(i32::try_from(error)?))
                .context("failed opening server process observation handle");
        }
        // SAFETY: successful OpenProcess returned a new owned handle.
        Ok(Self {
            handle: Some(unsafe { OwnedHandle::from_raw_handle(handle) }),
        })
    }

    #[cfg(windows)]
    fn exited_windows(&self) -> Result<bool> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        let Some(handle) = &self.handle else {
            return Ok(true);
        };
        // SAFETY: owned handle stays alive for this nonblocking wait.
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            WAIT_FAILED => {
                Err(std::io::Error::last_os_error()).context("failed polling server process handle")
            }
            result => anyhow::bail!("unexpected server process wait result: {result}"),
        }
    }

    pub(super) async fn wait(&self, deadline: tokio::time::Instant) -> Result<bool> {
        loop {
            match process_wait_decision(self.exited()?, tokio::time::Instant::now() >= deadline) {
                ProcessWaitDecision::Exited => return Ok(true),
                ProcessWaitDecision::TimedOut => return Ok(false),
                ProcessWaitDecision::Wait => {}
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + SERVER_POLL_INTERVAL),
            )
            .await;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProcessWaitDecision {
    Exited,
    TimedOut,
    Wait,
}

pub(super) const fn process_wait_decision(
    exited: bool,
    deadline_elapsed: bool,
) -> ProcessWaitDecision {
    if exited {
        ProcessWaitDecision::Exited
    } else if deadline_elapsed {
        ProcessWaitDecision::TimedOut
    } else {
        ProcessWaitDecision::Wait
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
pub(super) async fn wait_for_process_exit_async(pid: u32, timeout: Duration) -> Result<bool> {
    ProcessObservation::open(pid)?
        .wait(tokio::time::Instant::now() + timeout)
        .await
}

pub(super) fn server_pid_file_path() -> PathBuf {
    bmux_config::ConfigPaths::default().server_pid_file()
}

pub(super) fn write_server_pid_file(pid: u32) -> Result<()> {
    let path = server_pid_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed creating runtime dir {}", parent.display()))?;
    }
    publish_server_pid_file(&path, pid)
}

fn publish_server_pid_file(path: &std::path::Path, pid: u32) -> Result<()> {
    // Readers reclaim malformed PID files. Publish only a complete value so a
    // concurrent readiness/stop probe cannot unlink a partially written PID.
    let temporary = path.with_extension(format!("{}.next", std::process::id()));
    std::fs::write(&temporary, pid.to_string())
        .with_context(|| format!("failed staging pid file {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("failed publishing pid file {}", path.display()))
}

pub(super) fn read_server_pid_file() -> Result<Option<u32>> {
    let path = server_pid_file_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed reading pid file {}", path.display()));
        }
    };

    parse_pid_content(&content).map_or_else(
        || {
            let _ = remove_server_pid_file();
            Ok(None)
        },
        |pid| Ok(Some(pid)),
    )
}

pub(super) fn remove_server_pid_file() -> Result<()> {
    let path = server_pid_file_path();
    let remove_pid_result = match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed removing pid file {}", path.display()))
        }
    };
    let remove_metadata_result = remove_server_runtime_metadata_file();
    remove_pid_result.and(remove_metadata_result)
}

pub fn is_pid_running(pid: u32) -> Result<bool> {
    if pid == 0 {
        return Ok(false);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        Ok(!ProcessObservation::open(pid)?.exited()?)
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let output = ProcessCommand::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .context("failed inspecting server process state")?;
        let state = String::from_utf8_lossy(&output.stdout);
        Ok(output.status.success()
            && !state.trim().is_empty()
            && !state.trim_start().starts_with('Z'))
    }
}

pub(super) async fn cleanup_stale_pid_file() -> Result<()> {
    let Some(pid) = read_server_pid_file()? else {
        return Ok(());
    };

    if !is_pid_running(pid)? && !probe_server_running(ConnectionContext::new(Some("local"))).await?
    {
        remove_server_pid_file()?;
    }

    Ok(())
}

pub(super) fn parse_pid_content(content: &str) -> Option<u32> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<u32>().ok().filter(|pid| *pid > 0)
}
#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    use crate::runtime::attach::runtime::describe_timeout;
    use crate::runtime::server_commands::server_event_name;
    use bmux_config::ResolvedTimeout;

    #[test]
    fn pid_publication_never_exposes_partial_contents() {
        let root = std::env::temp_dir().join(format!("bmux-pid-publish-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("server.pid");
        publish_server_pid_file(&path, 111).unwrap();
        std::thread::scope(|scope| {
            let path = &path;
            scope.spawn(move || {
                for _ in 0..500 {
                    publish_server_pid_file(path, 222).unwrap();
                }
            });
            for _ in 0..500 {
                let content = std::fs::read_to_string(path).unwrap();
                assert!(matches!(parse_pid_content(&content), Some(111 | 222)));
            }
        });
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn describe_timeout_formats_resolved_timeout_states() {
        assert_eq!(describe_timeout(&ResolvedTimeout::Indefinite), "indefinite");
        assert_eq!(
            describe_timeout(&ResolvedTimeout::Exact(275)),
            "exact (275ms)"
        );
        assert_eq!(
            describe_timeout(&ResolvedTimeout::Profile {
                name: "traditional".to_string(),
                ms: 450,
            }),
            "profile:traditional (450ms)"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn process_exit_wait_does_not_report_a_live_process_as_stopped() {
        // No IPC endpoint exists for this child. IPC failure alone must not
        // authorize reporting a completed local shutdown.
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        assert!(
            !wait_for_process_exit_async(pid, Duration::from_millis(20))
                .await
                .unwrap()
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        assert!(
            wait_for_process_exit_async(pid, Duration::from_secs(1))
                .await
                .unwrap()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn process_exit_wait_accepts_an_unreaped_child() {
        let mut child = ProcessCommand::new("sleep").arg("0.1").spawn().unwrap();
        // Do not reap until after the wait, matching a foreground server whose
        // parent is synchronously waiting for the stop command to return.
        let exited = wait_for_process_exit_async(child.id(), Duration::from_secs(5)).await;
        let status = child.wait().unwrap();
        assert!(status.success());
        assert!(exited.unwrap());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn process_exit_wait_waits_for_delayed_exit() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("0.2")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let (exited, status) = tokio::join!(
            wait_for_process_exit_async(pid, Duration::from_secs(5)),
            child.wait(),
        );
        assert!(status.unwrap().success());
        assert!(exited.unwrap());
    }

    #[test]
    fn parse_pid_content_accepts_positive_pid() {
        assert_eq!(parse_pid_content("123\n"), Some(123));
    }

    #[test]
    fn parse_pid_content_rejects_invalid_values() {
        assert_eq!(parse_pid_content(""), None);
        assert_eq!(parse_pid_content("0"), None);
        assert_eq!(parse_pid_content("abc"), None);
    }

    #[test]
    fn server_event_name_maps_known_variants() {
        assert_eq!(
            server_event_name(&bmux_client::ServerEvent::ServerStarted),
            "server_started"
        );
        assert_eq!(
            server_event_name(&bmux_client::ServerEvent::ServerStopping),
            "server_stopping"
        );
    }
}
