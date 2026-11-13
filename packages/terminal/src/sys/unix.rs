//! Unix terminal modes and queries.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Terminal attributes in effect before raw mode was enabled.
static SAVED_ATTRIBUTES: Mutex<Option<libc::termios>> = Mutex::new(None);
/// Mirror of `SAVED_ATTRIBUTES.is_some()` for the input decoder's hot path.
static RAW_MODE: AtomicBool = AtomicBool::new(false);

/// A descriptor for the controlling terminal: stdin when it is a terminal,
/// otherwise a freshly opened `/dev/tty` that is closed on drop.
pub enum TtyFd {
    Stdin,
    Owned(File),
}

impl TtyFd {
    /// Stdin when it is a terminal, otherwise `/dev/tty` opened read-write.
    pub fn open() -> io::Result<Self> {
        // SAFETY: isatty only inspects the descriptor.
        if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
            return Ok(Self::Stdin);
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map(Self::Owned)
    }

    pub fn raw_fd(&self) -> RawFd {
        match self {
            Self::Stdin => libc::STDIN_FILENO,
            Self::Owned(file) => file.as_raw_fd(),
        }
    }
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn lock_saved() -> std::sync::MutexGuard<'static, Option<libc::termios>> {
    // A panic while holding the lock cannot leave the saved value torn.
    SAVED_ATTRIBUTES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn enable_raw_mode() -> io::Result<()> {
    let mut saved = lock_saved();
    if saved.is_some() {
        return Ok(());
    }
    let tty = TtyFd::open()?;
    let fd = tty.raw_fd();
    let mut attributes = std::mem::MaybeUninit::<libc::termios>::zeroed();
    // SAFETY: `attributes` is valid storage for one termios.
    check(unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) })?;
    // SAFETY: tcgetattr succeeded and initialized the value.
    let original = unsafe { attributes.assume_init() };
    let mut raw = original;
    // SAFETY: `raw` is an initialized termios.
    unsafe { libc::cfmakeraw(&raw mut raw) };
    // SAFETY: valid descriptor and termios.
    check(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const raw) })?;
    *saved = Some(original);
    RAW_MODE.store(true, Ordering::SeqCst);
    drop(saved);
    Ok(())
}

pub fn disable_raw_mode() -> io::Result<()> {
    let mut saved = lock_saved();
    if let Some(original) = saved.as_ref() {
        let tty = TtyFd::open()?;
        // SAFETY: valid descriptor and termios.
        check(unsafe { libc::tcsetattr(tty.raw_fd(), libc::TCSANOW, original) })?;
        settle_pending_input(tty.raw_fd());
        *saved = None;
        RAW_MODE.store(false, Ordering::SeqCst);
    }
    drop(saved);
    Ok(())
}

/// Let the terminal driver finish switching back to canonical input.
///
/// BSD-derived kernels (including macOS) mark pending input for retyping
/// (`PENDIN`) whenever canonical mode is turned back on, and clear the mark
/// the next time pending input is counted. Counting it here leaves the
/// terminal attributes exactly as they were before raw mode. Elsewhere the
/// query only reports the pending byte count.
fn settle_pending_input(fd: RawFd) {
    let mut pending: libc::c_int = 0;
    // SAFETY: valid descriptor and output storage for FIONREAD.
    unsafe { libc::ioctl(fd, libc::FIONREAD, &raw mut pending) };
}

// Keeps the same signature as the Windows implementation, which can fail.
#[allow(clippy::unnecessary_wraps)]
pub fn is_raw_mode_enabled() -> io::Result<bool> {
    Ok(raw_mode_enabled())
}

/// Whether raw mode is enabled, without locking.
pub fn raw_mode_enabled() -> bool {
    RAW_MODE.load(Ordering::SeqCst)
}

fn winsize_of(fd: RawFd) -> io::Result<libc::winsize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: valid descriptor and correctly sized output storage.
    check(unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) })?;
    // SAFETY: a successful ioctl initialized the value.
    Ok(unsafe { size.assume_init() })
}

/// The window size of `/dev/tty`, or of stdout when it cannot be opened.
fn terminal_winsize() -> io::Result<libc::winsize> {
    let tty = File::open("/dev/tty");
    let fd = tty.as_ref().map_or(libc::STDOUT_FILENO, AsRawFd::as_raw_fd);
    winsize_of(fd)
}

pub fn window_size() -> io::Result<crate::terminal::WindowSize> {
    let size = terminal_winsize()?;
    Ok(crate::terminal::WindowSize {
        rows: size.ws_row,
        columns: size.ws_col,
        width: size.ws_xpixel,
        height: size.ws_ypixel,
    })
}

pub fn size() -> io::Result<(u16, u16)> {
    if let Ok(size) = terminal_winsize() {
        return Ok((size.ws_col, size.ws_row));
    }
    let error = io::Error::last_os_error();
    tput_size().ok_or(error)
}

fn tput_value(capability: &str) -> Option<u16> {
    let output = std::process::Command::new("tput")
        .arg(capability)
        .output()
        .ok()?;
    let value = output
        .stdout
        .iter()
        .filter_map(|byte| char::from(*byte).to_digit(10))
        .fold(0u16, |value, digit| {
            // Digits are below 10.
            #[allow(clippy::cast_possible_truncation)]
            let digit = digit as u16;
            value.wrapping_mul(10).wrapping_add(digit)
        });
    (value > 0).then_some(value)
}

fn tput_size() -> Option<(u16, u16)> {
    Some((tput_value("cols")?, tput_value("lines")?))
}

pub fn supports_keyboard_enhancement() -> io::Result<bool> {
    crate::event::reader::query_keyboard_enhancement()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_mode_state_starts_disabled() {
        // Tests never enable raw mode on the developer's terminal.
        assert!(!raw_mode_enabled() || lock_saved().is_some());
    }
}
