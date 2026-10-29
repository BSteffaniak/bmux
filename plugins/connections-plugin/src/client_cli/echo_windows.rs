//! Console echo is restored on every prompt exit, including parse/read errors.
use anyhow::{Result, ensure};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};
pub(super) struct Echo {
    handle: HANDLE,
    mode: u32,
}
impl Echo {
    pub(super) fn hide() -> Result<Self> {
        // SAFETY: reads this process's standard console input handle and mode.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        ensure!(
            !handle.is_null() && handle != INVALID_HANDLE_VALUE,
            "console input unavailable"
        );
        let mut mode = 0;
        ensure!(
            unsafe { GetConsoleMode(handle, &raw mut mode) } != 0,
            "cannot read console input mode"
        );
        ensure!(
            unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } != 0,
            "cannot protect secret input"
        );
        Ok(Self { handle, mode })
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        // SAFETY: retained process-owned console handle; original mode only.
        unsafe { SetConsoleMode(self.handle, self.mode) };
    }
}
