//! Windows console modes and queries.

use std::io;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_EXTENDED_FLAGS,
    ENABLE_LINE_INPUT, ENABLE_MOUSE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, ENABLE_WINDOW_INPUT, GetConsoleMode,
    GetConsoleScreenBufferInfo, SetConsoleMode,
};

/// Input modes whose presence means the console is not in raw mode.
const COOKED_INPUT_MODES: CONSOLE_MODE =
    ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT;

/// Console input mode while mouse capture is enabled.
const MOUSE_CAPTURE_MODE: CONSOLE_MODE =
    ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT;

/// An owned console handle, closed on drop.
pub struct ConsoleHandle(HANDLE);

impl ConsoleHandle {
    fn open(name: &str) -> io::Result<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    /// The console input buffer.
    pub fn input() -> io::Result<Self> {
        Self::open("CONIN$")
    }

    /// The active console screen buffer.
    pub fn output() -> io::Result<Self> {
        Self::open("CONOUT$")
    }

    pub const fn raw(&self) -> HANDLE {
        self.0
    }

    pub fn mode(&self) -> io::Result<CONSOLE_MODE> {
        let mut mode = 0;
        // SAFETY: valid console handle and output pointer.
        if unsafe { GetConsoleMode(self.0, &raw mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    pub fn set_mode(&self, mode: CONSOLE_MODE) -> io::Result<()> {
        // SAFETY: valid console handle.
        if unsafe { SetConsoleMode(self.0, mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn screen_buffer_info(&self) -> io::Result<CONSOLE_SCREEN_BUFFER_INFO> {
        // SAFETY: plain data, fully written on success.
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: valid console handle and output pointer.
        if unsafe { GetConsoleScreenBufferInfo(self.0, &raw mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    }
}

impl Drop for ConsoleHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

pub fn enable_raw_mode() -> io::Result<()> {
    let input = ConsoleHandle::input()?;
    let mode = input.mode()?;
    input.set_mode(mode & !COOKED_INPUT_MODES)
}

pub fn disable_raw_mode() -> io::Result<()> {
    let input = ConsoleHandle::input()?;
    let mode = input.mode()?;
    input.set_mode(mode | COOKED_INPUT_MODES)
}

pub fn is_raw_mode_enabled() -> io::Result<bool> {
    Ok(ConsoleHandle::input()?.mode()? & COOKED_INPUT_MODES == 0)
}

/// The visible window of the active screen buffer as `(columns, rows)`.
pub fn size() -> io::Result<(u16, u16)> {
    let info = ConsoleHandle::output()?.screen_buffer_info()?;
    Ok(window_dimensions(&info))
}

pub fn window_dimensions(info: &CONSOLE_SCREEN_BUFFER_INFO) -> (u16, u16) {
    let window = info.srWindow;
    let columns = i32::from(window.Right) - i32::from(window.Left) + 1;
    let rows = i32::from(window.Bottom) - i32::from(window.Top) + 1;
    (
        u16::try_from(columns).unwrap_or(0),
        u16::try_from(rows).unwrap_or(0),
    )
}

// Keeps the same non-const signature as the Unix implementation, which
// queries the terminal and can fail.
#[allow(clippy::unnecessary_wraps, clippy::missing_const_for_fn)]
pub fn supports_keyboard_enhancement() -> io::Result<bool> {
    Ok(false)
}

/// Whether escape sequences reach a terminal that interprets them.
///
/// The first call enables virtual terminal processing on the console screen
/// buffer. Escape sequences are considered supported when that succeeds, or
/// when `TERM` names a terminal other than `dumb` (for terminals that are not
/// Windows consoles).
pub fn supports_ansi() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        enable_vt_processing().is_ok()
            || std::env::var("TERM").is_ok_and(|term| term != "dumb")
    })
}

fn enable_vt_processing() -> io::Result<()> {
    let output = ConsoleHandle::output()?;
    let mode = output.mode()?;
    if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING == 0 {
        output.set_mode(mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING)?;
    }
    Ok(())
}

/// Console input mode saved by the first mouse capture; `u64::MAX` until then.
static MODE_BEFORE_MOUSE_CAPTURE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Report mouse and window events and turn off quick-edit selection.
pub fn enable_mouse_capture() -> io::Result<()> {
    let input = ConsoleHandle::input()?;
    let mode = input.mode()?;
    // Only the first capture records the mode to restore.
    let _ = MODE_BEFORE_MOUSE_CAPTURE.compare_exchange(
        u64::MAX,
        u64::from(mode),
        Ordering::Relaxed,
        Ordering::Relaxed,
    );
    input.set_mode(MOUSE_CAPTURE_MODE)
}

/// Restore the input mode recorded by the first mouse capture.
pub fn disable_mouse_capture() -> io::Result<()> {
    let mode = u32::try_from(MODE_BEFORE_MOUSE_CAPTURE.load(Ordering::Relaxed))
        .map_err(|_| io::Error::other("console mode before mouse capture was never recorded"))?;
    ConsoleHandle::input()?.set_mode(mode)
}
