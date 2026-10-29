//! Fresh-process supervisor. Never runs a post-fork Rust loop in the daemon.
#[cfg(unix)]
#[path = "../atomic_state.rs"]
mod atomic_state;
#[path = "../supervisor_protocol.rs"]
mod protocol;
#[cfg(unix)]
#[path = "../supervisor_receipts.rs"]
mod receipts;

#[cfg(unix)]
#[path = "execution_supervisor/session_guardian.rs"]
mod session_guardian;

#[cfg(unix)]
#[path = "execution_supervisor/session_cleanup.rs"]
mod session_cleanup;

#[cfg(windows)]
#[path = "execution_supervisor/windows.rs"]
mod windows;
#[cfg(windows)]
#[path = "execution_supervisor/windows_job.rs"]
mod windows_job;
#[cfg(windows)]
#[path = "execution_supervisor/windows_security.rs"]
mod windows_security;

#[cfg(unix)]
mod unix {
    pub(super) fn private_peer(socket: &std::os::unix::net::UnixStream) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        #[cfg(target_os = "macos")]
        {
            let mut uid = 0;
            let mut gid = 0;
            // SAFETY: valid socket descriptor and initialized output pointers.
            if unsafe { libc::getpeereid(socket.as_raw_fd(), &mut uid, &mut gid) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: reading own effective UID has no side effect.
            if uid != unsafe { libc::geteuid() } {
                return Err(std::io::Error::other("supervisor peer owner mismatch"));
            }
        }
        #[cfg(target_os = "linux")]
        {
            let mut credential: libc::ucred = unsafe { std::mem::zeroed() };
            let mut length =
                u32::try_from(std::mem::size_of::<libc::ucred>()).map_err(std::io::Error::other)?;
            // SAFETY: buffer/length match SO_PEERCRED representation.
            if unsafe {
                libc::getsockopt(
                    socket.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    std::ptr::from_mut(&mut credential).cast(),
                    &mut length,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if credential.uid != unsafe { libc::geteuid() } {
                return Err(std::io::Error::other("supervisor peer owner mismatch"));
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        return Err(std::io::Error::other(
            "supervisor peer authentication unsupported on this platform",
        ));
        Ok(())
    }
}

fn main() {
    #[cfg(unix)]
    {
        let args: Vec<_> = std::env::args().collect();
        let result = match args.get(1).map(String::as_str) {
            Some("--terminal-owner") if args.len() == 4 => {
                session_guardian::owner(&args[2], &args[3])
            }
            Some("--join-and-kill-group") if args.len() == 3 => {
                session_cleanup::join_and_kill(&args[2])
            }
            Some("--guardian-terminal-session-v4") if args.len() == 5 => {
                session_guardian::run(&args[2], &args[3], &args[4])
            }
            _ => Err(std::io::Error::other(
                "unsupported execution supervisor invocation",
            )),
        };
        if result.is_err() {
            std::process::exit(1);
        }
    }
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args().collect();
        let result = match args.get(1).map(String::as_str) {
            Some("--guardian-windows-session-v1") if args.len() == 4 => {
                windows::run(&args[2], &args[3])
            }
            _ => Err(std::io::Error::other(
                "unsupported execution supervisor invocation",
            )),
        };
        if let Err(error) = result {
            // Never print argv, bootstrap contents, or the authentication secret.
            eprintln!("Windows execution supervisor failed: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(any(unix, windows)))]
    std::process::exit(1);
}
