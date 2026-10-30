//! An exclusively owned tty transport. Protocol replies are not rendering fences.
use crate::model::{BenchResult, Geometry};
use crate::runner::Backend;
use std::time::Duration;

#[cfg(unix)]
mod unix {
    use super::{Backend, BenchResult, Duration, Geometry};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, RawFd};
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Instant;

    const MAX_REPLY_BYTES: usize = 256;

    pub struct TerminalBackend {
        tty: File,
        original: libc::termios,
        probe_usable: bool,
        pending_probe: bool,
        restored: bool,
    }

    fn error(context: &str) -> String {
        format!("{context}: {}", io::Error::last_os_error())
    }

    fn wait(fd: RawFd, events: libc::c_short, deadline: Instant) -> BenchResult<bool> {
        #[cfg(target_os = "macos")]
        {
            // Darwin poll reports POLLNVAL for /dev/tty even though select and
            // ordinary I/O work. select is bounded by FD_SETSIZE; never index an
            // fd_set with an unchecked descriptor.
            if fd < 0 || fd as usize >= libc::FD_SETSIZE {
                return Err("tty descriptor exceeds select capacity".into());
            }
            loop {
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50));
                if remaining.is_zero() {
                    return Ok(false);
                }
                // SAFETY: zero is a valid empty fd_set; descriptor bounds checked above.
                let mut readers: libc::fd_set = unsafe { std::mem::zeroed() };
                let mut writers: libc::fd_set = unsafe { std::mem::zeroed() };
                unsafe {
                    libc::FD_ZERO(&raw mut readers);
                    libc::FD_ZERO(&raw mut writers);
                    if events & libc::POLLIN != 0 {
                        libc::FD_SET(fd, &raw mut readers);
                    }
                    if events & libc::POLLOUT != 0 {
                        libc::FD_SET(fd, &raw mut writers);
                    }
                }
                let mut timeout = libc::timeval {
                    tv_sec: 0,
                    tv_usec: remaining.as_micros().max(1) as libc::suseconds_t,
                };
                // SAFETY: live fd_sets and timeval for this synchronous call.
                let result = unsafe {
                    libc::select(
                        fd + 1,
                        &raw mut readers,
                        &raw mut writers,
                        std::ptr::null_mut(),
                        &raw mut timeout,
                    )
                };
                if result > 0 {
                    return Ok(true);
                }
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return Err(error("tty select"));
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            // Round up without ever extending the caller's deadline on EINTR.
            let milliseconds = remaining.as_millis().saturating_add(1).min(50);
            let mut descriptor = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            // SAFETY: descriptor is initialized and valid for one pollfd.
            let result = unsafe { libc::poll(&raw mut descriptor, 1, milliseconds as i32) };
            if result < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error("tty poll"));
            }
            if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(format!(
                    "tty disconnected or unavailable (poll events={events:#x}, revents={:#x})",
                    descriptor.revents
                ));
            }
            if result > 0 && descriptor.revents & events != 0 {
                return Ok(true);
            }
        }
    }

    impl TerminalBackend {
        pub fn open() -> BenchResult<Self> {
            let tty = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
                .open("/dev/tty")
                .map_err(|e| format!("open controlling tty: {e}"))?;
            // SAFETY: termios is plain C storage; tcgetattr initializes it on success.
            let mut original: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(tty.as_raw_fd(), &raw mut original) } != 0 {
                return Err(error("read tty attributes"));
            }
            let mut mode = original;
            // No alternate-screen, cursor, SGR, or synchronized-output changes here.
            // Keep signal handling owned by the frontend, including Ctrl-C.
            mode.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ECHONL);
            mode.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON);
            mode.c_oflag &= !libc::OPOST;
            mode.c_cc[libc::VMIN] = 0;
            mode.c_cc[libc::VTIME] = 0;
            let mut backend = Self {
                tty,
                original,
                probe_usable: true,
                pending_probe: false,
                restored: false,
            };
            if unsafe { libc::tcsetattr(backend.tty.as_raw_fd(), libc::TCSANOW, &raw const mode) }
                != 0
            {
                let failure = error("set tty attributes");
                return match backend.restore() {
                    Ok(()) => Err(failure),
                    Err(cleanup) => Err(format!("{failure}; {cleanup}")),
                };
            }
            Ok(backend)
        }
    }

    impl Backend for TerminalBackend {
        fn geometry(&mut self) -> BenchResult<Geometry> {
            // SAFETY: winsize is plain C storage; ioctl initializes it on success.
            let mut size: libc::winsize = unsafe { std::mem::zeroed() };
            if unsafe { libc::ioctl(self.tty.as_raw_fd(), libc::TIOCGWINSZ, &raw mut size) } != 0 {
                return Err(error("read tty geometry"));
            }
            if size.ws_col == 0 || size.ws_row == 0 {
                return Err("tty reported zero geometry".into());
            }
            Ok(Geometry {
                columns: size.ws_col,
                rows: size.ws_row,
            })
        }

        fn write(&mut self, mut bytes: &[u8], timeout: Duration) -> BenchResult<()> {
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or("write deadline overflow")?;
            while !bytes.is_empty() {
                if !wait(self.tty.as_raw_fd(), libc::POLLOUT, deadline)? {
                    return Err("tty write deadline exceeded (partial acceptance possible)".into());
                }
                // SAFETY: bytes is a live readable slice, fd is owned by self.
                let count = unsafe {
                    libc::write(self.tty.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                };
                if count < 0 {
                    let kind = io::Error::last_os_error().kind();
                    if matches!(kind, io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) {
                        continue;
                    }
                    return Err(error("tty write"));
                }
                if count == 0 {
                    return Err("tty write made no progress".into());
                }
                bytes = &bytes[usize::try_from(count).map_err(|_| "invalid tty write count")?..];
            }
            Ok(())
        }

        fn probe(&mut self, timeout: Duration) -> BenchResult<Option<Duration>> {
            if !self.probe_usable {
                return Ok(None);
            }
            // A timeout poisons subsequent probes: a delayed old reply is not attributable.
            self.probe_usable = false;
            let started = Instant::now();
            let deadline = started
                .checked_add(timeout)
                .ok_or("probe deadline overflow")?;
            // Reject pending input/replies rather than mistaking an old CPR for this query.
            #[cfg(not(target_os = "macos"))]
            {
                let mut pending = libc::pollfd {
                    fd: self.tty.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: pending is valid for one pollfd.
                if unsafe { libc::poll(&raw mut pending, 1, 0) } < 0 {
                    return Err(error("probe preflight"));
                }
                if pending.revents != 0 {
                    return Ok(None);
                }
            }
            #[cfg(target_os = "macos")]
            if wait(
                self.tty.as_raw_fd(),
                libc::POLLIN,
                Instant::now() + Duration::from_micros(1),
            )? {
                return Ok(None);
            }
            self.pending_probe = true;
            self.write(
                b"\x1b[6n",
                deadline.saturating_duration_since(Instant::now()),
            )?;
            let mut reply = Vec::with_capacity(MAX_REPLY_BYTES);
            while reply.len() < MAX_REPLY_BYTES {
                if !wait(self.tty.as_raw_fd(), libc::POLLIN, deadline)? {
                    self.probe_usable = false;
                    return Ok(None);
                }
                let mut byte = 0_u8;
                // SAFETY: byte is writable for one byte and fd is owned by self.
                let count = unsafe { libc::read(self.tty.as_raw_fd(), (&raw mut byte).cast(), 1) };
                if count < 0 {
                    if matches!(
                        io::Error::last_os_error().kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) {
                        continue;
                    }
                    return Err(error("read protocol reply"));
                }
                if count == 0 {
                    continue;
                }
                reply.push(byte);
                if !cursor_report_prefix(&reply) {
                    self.probe_usable = false;
                    return Ok(None);
                }
                if byte == b'R' {
                    self.probe_usable = valid_cursor_report(&reply);
                    self.pending_probe = !self.probe_usable;
                    return Ok(self.probe_usable.then(|| started.elapsed()));
                }
            }
            self.probe_usable = false;
            Ok(None)
        }

        fn restore(&mut self) -> BenchResult<()> {
            let unresolved = self.pending_probe;
            if unresolved {
                // Bounded quarantine of a delayed reply. Never issue another query
                // or blindly flush unrelated input while recovering.
                let deadline = Instant::now() + Duration::from_millis(250);
                let mut reply = Vec::with_capacity(MAX_REPLY_BYTES);
                while reply.len() < MAX_REPLY_BYTES
                    && wait(self.tty.as_raw_fd(), libc::POLLIN, deadline).unwrap_or(false)
                {
                    let mut byte = 0_u8;
                    // SAFETY: owned descriptor and one writable byte.
                    if unsafe { libc::read(self.tty.as_raw_fd(), (&raw mut byte).cast(), 1) } != 1 {
                        break;
                    }
                    reply.push(byte);
                    if !cursor_report_prefix(&reply) {
                        break;
                    }
                    if byte == b'R' {
                        self.pending_probe = !valid_cursor_report(&reply);
                        break;
                    }
                }
            }
            if !self.restored {
                // Restore only termios that this backend changed, not display modes.
                if unsafe {
                    libc::tcsetattr(
                        self.tty.as_raw_fd(),
                        libc::TCSANOW,
                        &raw const self.original,
                    )
                } != 0
                {
                    return Err(error("restore tty attributes"));
                }
                self.restored = true;
            }
            if self.pending_probe {
                Err("terminal modes restored, but a protocol reply remains unresolved; delayed reply safety cannot be established (probes are opt-in)".into())
            } else {
                Ok(())
            }
        }
    }

    impl Drop for TerminalBackend {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    fn cursor_report_prefix(bytes: &[u8]) -> bool {
        if bytes.len() <= 2 {
            return b"\x1b[".starts_with(bytes);
        }
        if !bytes.starts_with(b"\x1b[") || bytes.len() > 14 {
            return false;
        }
        let body = &bytes[2..];
        let body = body.strip_suffix(b"R").unwrap_or(body);
        let mut separators = 0;
        let mut digits = 0;
        for byte in body {
            if *byte == b';' {
                if separators != 0 || digits == 0 {
                    return false;
                }
                separators += 1;
                digits = 0;
            } else if byte.is_ascii_digit() {
                digits += 1;
                if digits > 5 {
                    return false;
                }
            } else {
                return false;
            }
        }
        true
    }

    fn valid_cursor_report(bytes: &[u8]) -> bool {
        let Some(body) = bytes
            .strip_prefix(b"\x1b[")
            .and_then(|s| s.strip_suffix(b"R"))
        else {
            return false;
        };
        let mut parts = body.split(|byte| *byte == b';');
        let valid_number = |part: &[u8]| {
            !part.is_empty()
                && part.len() <= 5
                && part.iter().all(u8::is_ascii_digit)
                && std::str::from_utf8(part)
                    .ok()
                    .and_then(|s| s.parse::<u16>().ok())
                    .is_some_and(|n| n > 0)
        };
        parts.next().is_some_and(valid_number)
            && parts.next().is_some_and(valid_number)
            && parts.next().is_none()
    }

    #[cfg(test)]
    mod tests {
        use super::{cursor_report_prefix, valid_cursor_report};
        #[test]
        fn reply_prefix_rejects_user_input_without_unbounded_scanning() {
            for prefix in [
                b"\x1b".as_slice(),
                b"\x1b[",
                b"\x1b[1",
                b"\x1b[1;",
                b"\x1b[1;2R",
            ] {
                assert!(cursor_report_prefix(prefix));
            }
            for invalid in [
                b"x".as_slice(),
                b"\x1bx",
                b"\x1b[;",
                b"\x1b[123456",
                b"\x1b[1;;",
                b"\x1b[?1",
            ] {
                assert!(!cursor_report_prefix(invalid));
            }
        }
        #[test]
        fn cursor_report_is_strict_and_bounded() {
            assert!(valid_cursor_report(b"\x1b[24;80R"));
            for invalid in [
                b"\x1b[0;1R".as_slice(),
                b"\x1b[1;2;3R",
                b"x\x1b[1;2R",
                b"\x1b[999999;1R",
                b"\x1b[?1;2R",
                b"\x1b[;2R",
            ] {
                assert!(!valid_cursor_report(invalid));
            }
        }
    }
}

#[cfg(unix)]
pub use unix::TerminalBackend;

#[cfg(not(unix))]
pub struct TerminalBackend;
#[cfg(not(unix))]
impl TerminalBackend {
    pub fn open() -> BenchResult<Self> {
        Err("real-terminal benchmarks are unsupported on this platform".into())
    }
}
#[cfg(not(unix))]
impl Backend for TerminalBackend {
    fn geometry(&mut self) -> BenchResult<Geometry> {
        Err("unsupported terminal backend".into())
    }
    fn write(&mut self, _: &[u8], _: Duration) -> BenchResult<()> {
        Err("unsupported terminal backend".into())
    }
    fn probe(&mut self, _: Duration) -> BenchResult<Option<Duration>> {
        Ok(None)
    }
    fn restore(&mut self) -> BenchResult<()> {
        Ok(())
    }
}
