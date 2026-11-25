use anyhow::{Result, ensure};
// Restore terminal echo on success and every error path. No shell subprocess or
// command-line argument ever contains the invitation.
pub(super) struct Echo(libc::termios);
impl Drop for Echo {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const self.0);
        }
    }
}
impl Echo {
    pub(super) fn hide() -> Result<Self> {
        let mut old = std::mem::MaybeUninit::<libc::termios>::uninit();
        ensure!(
            unsafe { libc::tcgetattr(libc::STDIN_FILENO, old.as_mut_ptr()) } == 0,
            "cannot protect secret input"
        );
        let old = unsafe { old.assume_init() };
        let mut hidden = old;
        hidden.c_lflag &= !libc::ECHO;
        ensure!(
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw const hidden) } == 0,
            "cannot protect secret input"
        );
        Ok(Self(old))
    }
}
