//! The process-wide input reader shared by `poll`, `read`, `EventStream`, and
//! capability probes.

use std::collections::VecDeque;
use std::io;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::Event;

/// Everything the input source can decode, including replies to queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InternalEvent {
    /// A user-facing event.
    Event(Event),
    /// A reply to the keyboard enhancement flags query (`CSI ? flags u`).
    #[cfg_attr(windows, allow(dead_code))]
    KeyboardEnhancementFlags,
    /// A primary device attributes reply (`CSI ? ... c`).
    #[cfg_attr(windows, allow(dead_code))]
    PrimaryDeviceAttributes,
    /// A cursor position report, zero-based `(column, row)`.
    #[cfg_attr(windows, allow(dead_code))]
    CursorPosition(u16, u16),
}

/// Selects which internal events a caller is waiting for.
#[derive(Clone, Copy)]
pub enum Filter {
    /// User-facing events.
    Event,
    /// Keyboard enhancement flags or primary device attributes replies.
    #[cfg_attr(windows, allow(dead_code))]
    KeyboardEnhancementReply,
    /// Primary device attributes replies.
    #[cfg_attr(windows, allow(dead_code))]
    PrimaryDeviceAttributes,
}

impl Filter {
    const fn accepts(self, event: &InternalEvent) -> bool {
        match self {
            Self::Event => matches!(event, InternalEvent::Event(_)),
            Self::KeyboardEnhancementReply => matches!(
                event,
                InternalEvent::KeyboardEnhancementFlags | InternalEvent::PrimaryDeviceAttributes
            ),
            Self::PrimaryDeviceAttributes => {
                matches!(event, InternalEvent::PrimaryDeviceAttributes)
            }
        }
    }
}

/// A platform input source.
pub trait Source: Send {
    /// Wait up to `timeout` (forever when `None`) for one decoded event.
    ///
    /// Returns `Ok(None)` on timeout and an `Interrupted` error when woken.
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>>;

    /// A handle that interrupts a blocked [`Source::try_read`].
    fn waker(&self) -> Waker;
}

/// Interrupts a blocked source read from any thread.
#[derive(Clone)]
pub struct Waker(std::sync::Arc<dyn Fn() + Send + Sync>);

impl Waker {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(wake))
    }

    pub fn wake(&self) {
        (self.0)();
    }
}

/// Decoded events plus the source that produces them.
pub struct Reader {
    source: Option<Box<dyn Source>>,
    source_error: Option<String>,
    events: VecDeque<InternalEvent>,
    skipped: Vec<InternalEvent>,
}

impl Reader {
    fn new() -> Self {
        let (source, source_error) = match open_source() {
            Ok(source) => (Some(source), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            source,
            source_error,
            events: VecDeque::with_capacity(32),
            skipped: Vec::with_capacity(32),
        }
    }

    pub fn waker(&self) -> Option<Waker> {
        self.source.as_ref().map(|source| source.waker())
    }

    /// Wait up to `timeout` for an event `filter` accepts, keeping every
    /// other decoded event queued in arrival order.
    pub fn poll(&mut self, timeout: Option<Duration>, filter: Filter) -> io::Result<bool> {
        if self.events.iter().any(|event| filter.accepts(event)) {
            return Ok(true);
        }
        let Some(source) = self.source.as_mut() else {
            return Err(io::Error::other(format!(
                "terminal input unavailable: {}",
                self.source_error.as_deref().unwrap_or("source not initialized")
            )));
        };
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let leftover =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let accepted = match source.try_read(leftover) {
                Ok(None) => None,
                Ok(Some(event)) if filter.accepts(&event) => Some(event),
                Ok(Some(event)) => {
                    self.skipped.push(event);
                    None
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    self.events.extend(self.skipped.drain(..));
                    return Ok(false);
                }
                Err(error) => {
                    self.events.extend(self.skipped.drain(..));
                    return Err(error);
                }
            };
            let expired = deadline.is_some_and(|deadline| Instant::now() >= deadline);
            if expired || accepted.is_some() {
                self.events.extend(self.skipped.drain(..));
                if let Some(event) = accepted {
                    self.events.push_front(event);
                    return Ok(true);
                }
                return Ok(false);
            }
        }
    }

    /// Block until an event `filter` accepts is available and remove it.
    pub fn read(&mut self, filter: Filter) -> io::Result<InternalEvent> {
        loop {
            if let Some(index) = self.events.iter().position(|event| filter.accepts(event))
                && let Some(event) = self.events.remove(index)
            {
                return Ok(event);
            }
            // Interrupted waits report `false`; keep waiting.
            let _ = self.poll(None, filter)?;
        }
    }
}

#[cfg(unix)]
fn open_source() -> io::Result<Box<dyn Source>> {
    Ok(Box::new(super::unix_source::UnixSource::new()?))
}

#[cfg(windows)]
fn open_source() -> io::Result<Box<dyn Source>> {
    Ok(Box::new(super::windows_source::WindowsSource::new()?))
}

/// A mutex whose acquisition can give up after a timeout.
pub struct TimedLock<T> {
    held: Mutex<bool>,
    released: Condvar,
    value: std::cell::UnsafeCell<T>,
}

// SAFETY: access to `value` is serialized by the `held` flag.
unsafe impl<T: Send> Sync for TimedLock<T> {}

pub struct TimedGuard<'a, T> {
    lock: &'a TimedLock<T>,
}

impl<T> TimedLock<T> {
    const fn new(value: T) -> Self {
        Self {
            held: Mutex::new(false),
            released: Condvar::new(),
            value: std::cell::UnsafeCell::new(value),
        }
    }

    fn held(&self) -> MutexGuard<'_, bool> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Acquire, waiting as long as needed.
    pub fn lock(&self) -> TimedGuard<'_, T> {
        let mut held = self.held();
        while *held {
            held = self
                .released
                .wait(held)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *held = true;
        drop(held);
        TimedGuard { lock: self }
    }

    /// Acquire, giving up after `timeout`.
    pub fn lock_for(&self, timeout: Duration) -> Option<TimedGuard<'_, T>> {
        let deadline = Instant::now() + timeout;
        let mut held = self.held();
        while *held {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            held = self
                .released
                .wait_timeout(held, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        *held = true;
        drop(held);
        Some(TimedGuard { lock: self })
    }
}

impl<T> std::ops::Deref for TimedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard holds exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> std::ops::DerefMut for TimedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard holds exclusive access.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for TimedGuard<'_, T> {
    fn drop(&mut self) {
        *self.lock.held() = false;
        self.lock.released.notify_one();
    }
}

/// The process-wide reader, created on first use.
fn global() -> &'static TimedLock<Option<Reader>> {
    static READER: OnceLock<TimedLock<Option<Reader>>> = OnceLock::new();
    READER.get_or_init(|| TimedLock::new(None))
}

/// Run `operation` on the global reader, creating it if needed.
fn with_reader<R>(guard: &mut TimedGuard<'_, Option<Reader>>, operation: impl FnOnce(&mut Reader) -> R) -> R {
    operation(guard.get_or_insert_with(Reader::new))
}

/// Lock the reader, waiting at most `timeout` when one is given.
fn acquire(timeout: Option<Duration>) -> Option<(TimedGuard<'static, Option<Reader>>, Option<Duration>)> {
    match timeout {
        Some(timeout) => {
            let started = Instant::now();
            let guard = global().lock_for(timeout)?;
            Some((guard, Some(timeout.saturating_sub(started.elapsed()))))
        }
        None => Some((global().lock(), None)),
    }
}

pub fn poll_internal(timeout: Option<Duration>, filter: Filter) -> io::Result<bool> {
    let Some((mut guard, leftover)) = acquire(timeout) else {
        return Ok(false);
    };
    with_reader(&mut guard, |reader| reader.poll(leftover, filter))
}

pub fn read_internal(filter: Filter) -> io::Result<InternalEvent> {
    let mut guard = global().lock();
    with_reader(&mut guard, |reader| reader.read(filter))
}

pub fn poll_event(timeout: Option<Duration>) -> io::Result<bool> {
    poll_internal(timeout, Filter::Event)
}

pub fn read_event() -> io::Result<Event> {
    match read_internal(Filter::Event)? {
        InternalEvent::Event(event) => Ok(event),
        _ => unreachable!("the event filter only accepts user-facing events"),
    }
}

/// The waker of the global reader's source, creating the reader if needed.
pub fn global_waker() -> Option<Waker> {
    let mut guard = global().lock();
    with_reader(&mut guard, |reader| reader.waker())
}

/// Query whether the terminal answers the kitty keyboard flags query.
#[cfg(unix)]
pub fn query_keyboard_enhancement() -> io::Result<bool> {
    if crate::sys::unix::raw_mode_enabled() {
        query_keyboard_enhancement_raw()
    } else {
        crate::sys::unix::enable_raw_mode()?;
        let supported = query_keyboard_enhancement_raw();
        crate::sys::unix::disable_raw_mode()?;
        supported
    }
}

#[cfg(unix)]
fn query_keyboard_enhancement_raw() -> io::Result<bool> {
    use std::io::Write;

    // Flags query, then primary device attributes: every terminal answers the
    // second, so a terminal without the protocol replies only to it.
    const QUERY: &[u8] = b"\x1b[?u\x1b[c";

    let written = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .and_then(|mut tty| {
            tty.write_all(QUERY)?;
            tty.flush()
        });
    if written.is_err() {
        let mut stdout = io::stdout();
        stdout.write_all(QUERY)?;
        stdout.flush()?;
    }

    loop {
        match poll_internal(Some(Duration::from_secs(2)), Filter::KeyboardEnhancementReply) {
            Ok(true) => {
                return match read_internal(Filter::KeyboardEnhancementReply) {
                    Ok(InternalEvent::KeyboardEnhancementFlags) => {
                        // Consume the device attributes reply that follows.
                        let _ = read_internal(Filter::PrimaryDeviceAttributes);
                        Ok(true)
                    }
                    _ => Ok(false),
                };
            }
            Ok(false) => {
                return Err(io::Error::other(
                    "the keyboard enhancement status could not be read within two seconds",
                ));
            }
            // Retry after transient source errors, as a wait that was woken.
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn timed_lock_gives_up_while_held() {
        let lock = Arc::new(TimedLock::new(0u32));
        let guard = lock.lock();
        assert!(lock.lock_for(Duration::from_millis(10)).is_none());
        drop(guard);
        let mut guard = lock.lock_for(Duration::from_millis(10)).expect("released");
        *guard += 1;
        drop(guard);
        assert_eq!(*lock.lock(), 1);
    }

    #[test]
    fn timed_lock_hands_over_between_threads() {
        let lock = Arc::new(TimedLock::new(0u32));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let lock = Arc::clone(&lock);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        *lock.lock() += 1;
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(*lock.lock(), 4000);
    }

    struct Scripted(VecDeque<io::Result<Option<InternalEvent>>>);

    impl Source for Scripted {
        fn try_read(&mut self, _: Option<Duration>) -> io::Result<Option<InternalEvent>> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
        fn waker(&self) -> Waker {
            Waker::new(|| {})
        }
    }

    fn reader(script: Vec<io::Result<Option<InternalEvent>>>) -> Reader {
        Reader {
            source: Some(Box::new(Scripted(script.into()))),
            source_error: None,
            events: VecDeque::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn filtered_reads_keep_other_events_queued_in_order() {
        let mut reader = reader(vec![
            Ok(Some(InternalEvent::Event(Event::FocusGained))),
            Ok(Some(InternalEvent::PrimaryDeviceAttributes)),
            Ok(Some(InternalEvent::Event(Event::FocusLost))),
        ]);
        assert_eq!(
            reader.read(Filter::KeyboardEnhancementReply).unwrap(),
            InternalEvent::PrimaryDeviceAttributes
        );
        assert_eq!(
            reader.read(Filter::Event).unwrap(),
            InternalEvent::Event(Event::FocusGained)
        );
        assert_eq!(
            reader.read(Filter::Event).unwrap(),
            InternalEvent::Event(Event::FocusLost)
        );
    }

    #[test]
    fn interrupted_poll_reports_no_event() {
        let mut reader = reader(vec![Err(io::Error::new(io::ErrorKind::Interrupted, "woken"))]);
        assert!(!reader.poll(None, Filter::Event).unwrap());
    }

    #[test]
    fn poll_times_out_without_events() {
        let mut reader = reader(vec![]);
        assert!(!reader.poll(Some(Duration::ZERO), Filter::Event).unwrap());
    }
}
