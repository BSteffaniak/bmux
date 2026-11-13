//! Unix-specific terminal support.

pub mod sigwinch {
    //! Delivery of `SIGWINCH` to registered descriptors.
    //!
    //! One process-wide handler writes a byte to every registered descriptor
    //! (typically the write end of a nonblocking socket pair) whenever the
    //! terminal is resized, then chains to any handler installed before it.
    //! The handler is installed on first registration and stays installed.

    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
    use std::sync::{Mutex, Once, OnceLock, PoisonError};

    const SLOTS: usize = 64;
    const EMPTY: i32 = -1;

    static TARGETS: [AtomicI32; SLOTS] = [const { AtomicI32::new(EMPTY) }; SLOTS];
    /// Handler invocations currently running, on any thread.
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    /// Serializes slot allocation and release.
    static SLOT_LOCK: Mutex<()> = Mutex::new(());
    static INSTALL: Once = Once::new();
    static INSTALL_RESULT: OnceLock<Result<(), i32>> = OnceLock::new();
    static PREVIOUS: OnceLock<libc::sigaction> = OnceLock::new();

    /// Keeps a descriptor registered; dropping it unregisters and closes it.
    #[derive(Debug)]
    pub struct Registration {
        slot: usize,
        fd: Option<OwnedFd>,
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            let _slots = SLOT_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
            TARGETS[self.slot].store(EMPTY, Ordering::SeqCst);
            // A handler that loaded the descriptor before it was cleared may
            // still write to it; close only once no handler is running.
            while RUNNING.load(Ordering::SeqCst) != 0 {
                std::hint::spin_loop();
            }
            drop(self.fd.take());
        }
    }

    /// Register `fd` to receive one byte per `SIGWINCH`.
    ///
    /// The descriptor should be nonblocking so a full buffer never blocks the
    /// handler; a full buffer already holds a pending notification.
    ///
    /// # Errors
    ///
    /// Returns any error installing the signal handler, or `OutOfMemory` when
    /// every registration slot is in use.
    pub fn register(fd: OwnedFd) -> io::Result<Registration> {
        install()?;
        let _slots = SLOT_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let raw = fd.as_raw_fd();
        for (slot, target) in TARGETS.iter().enumerate() {
            if target
                .compare_exchange(EMPTY, raw, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(Registration { slot, fd: Some(fd) });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "too many SIGWINCH registrations",
        ))
    }

    fn install() -> io::Result<()> {
        INSTALL.call_once(|| {
            let result = install_handler();
            let _ = INSTALL_RESULT.set(result);
        });
        match INSTALL_RESULT.get() {
            Some(Ok(())) => Ok(()),
            Some(Err(code)) => Err(io::Error::from_raw_os_error(*code)),
            None => Err(io::Error::other("SIGWINCH handler installation failed")),
        }
    }

    fn install_handler() -> Result<(), i32> {
        // SAFETY: zeroed sigaction is a valid starting value.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
        // SAFETY: valid signal set pointer.
        unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        // SAFETY: zeroed sigaction is valid output storage.
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        // Record the previous action before the handler can run.
        // SAFETY: valid pointers; only queries the current action.
        if unsafe { libc::sigaction(libc::SIGWINCH, std::ptr::null(), &raw mut previous) } != 0 {
            return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        let _ = PREVIOUS.set(previous);
        // SAFETY: valid action; the handler is async-signal-safe.
        if unsafe { libc::sigaction(libc::SIGWINCH, &raw const action, std::ptr::null_mut()) } != 0
        {
            return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        Ok(())
    }

    extern "C" fn handler(
        signal: libc::c_int,
        info: *mut libc::siginfo_t,
        context: *mut libc::c_void,
    ) {
        RUNNING.fetch_add(1, Ordering::SeqCst);
        let saved_errno = errno();
        for target in &TARGETS {
            let fd = target.load(Ordering::SeqCst);
            if fd != EMPTY {
                let byte = 1u8;
                // SAFETY: write is async-signal-safe; the descriptor stays
                // open while RUNNING is nonzero.
                unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
            }
        }
        set_errno(saved_errno);
        RUNNING.fetch_sub(1, Ordering::SeqCst);
        chain(signal, info, context);
    }

    /// Run the handler that was installed before ours, if it was a function.
    fn chain(signal: libc::c_int, info: *mut libc::siginfo_t, context: *mut libc::c_void) {
        let Some(previous) = PREVIOUS.get() else {
            return;
        };
        let target = previous.sa_sigaction;
        if target == libc::SIG_DFL || target == libc::SIG_IGN {
            return;
        }
        if previous.sa_flags & libc::SA_SIGINFO != 0 {
            // SAFETY: the previous action declared a three-argument handler.
            let function: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                unsafe { std::mem::transmute(target) };
            function(signal, info, context);
        } else {
            // SAFETY: the previous action declared a one-argument handler.
            let function: extern "C" fn(libc::c_int) = unsafe { std::mem::transmute(target) };
            function(signal);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: returns the calling thread's errno location.
        unsafe { libc::__errno_location() }
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: returns the calling thread's errno location.
        unsafe { libc::__error() }
    }

    #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
    fn errno_location() -> *mut libc::c_int {
        // SAFETY: returns the calling thread's errno location.
        unsafe { libc::__errno() }
    }

    fn errno() -> libc::c_int {
        // SAFETY: the location is valid for the calling thread.
        unsafe { *errno_location() }
    }

    fn set_errno(value: libc::c_int) {
        // SAFETY: the location is valid for the calling thread.
        unsafe { *errno_location() = value };
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Read;
        use std::os::unix::net::UnixStream;

        #[test]
        fn registered_descriptors_receive_resizes_until_dropped() {
            let (mut receiver, sender) = UnixStream::pair().unwrap();
            receiver.set_nonblocking(true).unwrap();
            sender.set_nonblocking(true).unwrap();
            let registration = register(OwnedFd::from(sender)).unwrap();
            // SAFETY: raising a signal whose handler is installed.
            unsafe { libc::raise(libc::SIGWINCH) };
            let mut byte = [0u8; 8];
            assert!(receiver.read(&mut byte).unwrap() >= 1);
            drop(registration);
            // SAFETY: as above.
            unsafe { libc::raise(libc::SIGWINCH) };
            // The write end is closed: the read reports end of stream.
            assert_eq!(receiver.read(&mut byte).unwrap(), 0);
        }
    }
}
