//! Instructions retired, the CPU cost of a benchmark that machine load barely changes (wall time
//! on a busy machine varies more than the differences being measured).
//!
//! On Linux the count is this thread's user-space instructions (a `perf_event_open` counter,
//! unavailable when `kernel.perf_event_paranoid` forbids it or the machine exposes no hardware
//! counters); on macOS it is the whole process's (`proc_pid_rusage`), so measure work on one
//! thread with nothing else running. Elsewhere there is no counter.

/// A source of instructions retired.
pub struct InstructionCounter {
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
}

impl InstructionCounter {
    /// A counter for this thread (this process on macOS), if the platform provides one.
    #[must_use]
    pub fn new() -> Option<Self> {
        let counter = Self::open()?;
        (counter.read() > 0).then_some(counter)
    }

    /// Run `work` and return the instructions retired while it ran.
    pub fn measure<T>(&self, work: impl FnOnce() -> T) -> (T, u64) {
        let before = self.read();
        let value = work();
        (value, self.read().saturating_sub(before))
    }

    #[cfg(target_os = "macos")]
    fn open() -> Option<Self> {
        Some(Self {})
    }

    /// Instructions retired so far.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn read(&self) -> u64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
        // SAFETY: `usage` is a writable `rusage_info_v4`, the struct `RUSAGE_INFO_V4` fills.
        let result = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V4,
                usage.as_mut_ptr().cast::<libc::rusage_info_t>(),
            )
        };
        if result != 0 {
            return 0;
        }
        // SAFETY: a successful call filled the struct.
        unsafe { usage.assume_init() }.ri_instructions
    }

    #[cfg(target_os = "linux")]
    fn open() -> Option<Self> {
        use std::os::fd::FromRawFd;
        let attr = PerfEventAttr {
            kind: 0,   // PERF_TYPE_HARDWARE
            config: 1, // PERF_COUNT_HW_INSTRUCTIONS
            size: u32::try_from(std::mem::size_of::<PerfEventAttr>()).ok()?,
            flags: EXCLUDE_KERNEL | EXCLUDE_HV,
            ..PerfEventAttr::default()
        };
        // SAFETY: `attr` is a valid `perf_event_attr` (version 0 layout) living through the call;
        // pid 0 / cpu -1 counts this thread on any CPU, with no group and no flags.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                std::ptr::from_ref(&attr),
                0,
                -1,
                -1,
                0,
            )
        };
        let fd = i32::try_from(fd).ok().filter(|fd| *fd >= 0)?;
        // SAFETY: the kernel returned a new descriptor this counter now owns.
        Some(Self {
            fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) },
        })
    }

    /// Instructions retired so far.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn read(&self) -> u64 {
        use std::os::fd::AsRawFd;
        let mut count = 0_u64;
        // SAFETY: reads eight bytes into `count` from a descriptor this counter owns.
        let read = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                std::ptr::from_mut(&mut count).cast(),
                std::mem::size_of::<u64>(),
            )
        };
        if read == 8 { count } else { 0 }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn open() -> Option<Self> {
        None
    }

    /// Instructions retired so far.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[must_use]
    pub fn read(&self) -> u64 {
        0
    }
}

#[cfg(target_os = "linux")]
const EXCLUDE_KERNEL: u64 = 1 << 5;
#[cfg(target_os = "linux")]
const EXCLUDE_HV: u64 = 1 << 6;

/// `struct perf_event_attr`, its first (version 0, 64-byte) layout: what a counting event needs.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
struct PerfEventAttr {
    kind: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
}
