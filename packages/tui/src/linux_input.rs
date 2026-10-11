//! Linux-owned terminal ingress.
//!
//! This is an opt-in legacy keyboard backend, not
//! a runtime replacement for a live Crossterm reader. Kitty/CSI-u, mouse, focus,
//! and capability-response decoding are deliberately unsupported. Do not issue
//! input-reading terminal probes with this backend. Resize is application-owned.
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::event::Event;
use bmux_keyboard::{KeyCode, KeyStroke, legacy};
use zeroize::Zeroize;

static OWNED: AtomicBool = AtomicBool::new(false);
const CAPACITY: usize = 8192;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Secret-free backend errors. All errors quarantine ingress until a reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxInputError {
    /// An in-process owner already exists.
    Busy,
    /// The terminal or operating-system operation failed.
    Terminal,
    /// Unsupported or malformed input protocol.
    Protocol,
    /// Bounded storage or draining budget was exhausted.
    Budget,
    /// Ingress is fenced; no public or private reads are admitted.
    Paused,
}

struct Ownership;
impl Ownership {
    fn acquire() -> Result<Self, LinuxInputError> {
        OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| LinuxInputError::Busy)
    }
}
impl Drop for Ownership {
    fn drop(&mut self) {
        OWNED.store(false, Ordering::Release);
    }
}

/// An event tagged with its reader epoch; never Debug-print private input.
pub struct LinuxInputEvent {
    /// Epoch used to reject an in-flight event after an application fence.
    pub epoch: u64,
    /// Route to the private gate before any diagnostics or ordinary admission.
    event: Event,
}
impl LinuxInputEvent {
    /// Admit only at the serialized application boundary using its CURRENT
    /// acknowledged epoch. Stale events are wiped rather than replayed.
    /// Route the returned event directly into the private gate before logging.
    #[allow(clippy::must_use_candidate)] // Option already carries must-use.
    pub fn take_for_epoch(mut self, current_epoch: u64) -> Option<Event> {
        if self.epoch != current_epoch {
            return None;
        }
        Some(std::mem::replace(&mut self.event, Event::Tick))
    }
}
impl LinuxInputEvent {
    fn wipe(&mut self) {
        match &mut self.event {
            Event::Paste(text) | Event::User(text) => text.zeroize(),
            Event::Key(stroke) => stroke.key = KeyCode::Escape,
            _ => {}
        }
    }
}
impl Drop for LinuxInputEvent {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// One raw reader, fixed parser storage, and original terminal mode.
/// No Debug implementation: parser storage must never enter diagnostics.
pub struct LinuxInputReader {
    tty: File,
    original: libc::termios,
    decoder: Decoder,
    paused: bool,
    epoch: u64,
    _ownership: Ownership,
}
impl LinuxInputReader {
    /// Open the controlling terminal in nonblocking raw mode from startup.
    ///
    /// # Safety
    /// The application must not have created ANY other terminal input reader,
    /// including Crossterm event streams, polling, reads or capability probes.
    /// It must use this owner for the entire terminal lifetime. The in-process
    /// singleton does not detect readers in other libraries or processes.
    /// # Errors
    /// Returns Busy for a second owner, Terminal on open/termios failure.
    pub unsafe fn from_startup() -> Result<Self, LinuxInputError> {
        let ownership = Ownership::acquire()?;
        let tty = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/tty")
            .map_err(|_| LinuxInputError::Terminal)?;
        // SAFETY: startup exclusivity is the caller's obligation; this function
        // keeps ownership of the only descriptor used to read input.
        Self::from_owned_file(tty, ownership)
    }
    fn from_owned_file(tty: File, ownership: Ownership) -> Result<Self, LinuxInputError> {
        // SAFETY: initialized storage and live terminal descriptor.
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), &raw mut original) } != 0 {
            return Err(LinuxInputError::Terminal);
        }
        let mut raw = original;
        // SAFETY: termios came from tcgetattr; descriptor remains owned.
        unsafe {
            libc::cfmakeraw(&raw mut raw);
        }
        if unsafe { libc::tcsetattr(tty.as_raw_fd(), libc::TCSANOW, &raw const raw) } != 0 {
            return Err(LinuxInputError::Terminal);
        }
        Ok(Self {
            tty,
            original,
            decoder: Decoder::new(),
            paused: true,
            epoch: 0,
            _ownership: ownership,
        })
    }

    /// Pause ingress before suppressing recording/tracing and fencing application
    /// event queues. This owner never logs or records input itself.
    pub const fn pause(&mut self) {
        self.paused = true;
    }

    /// Wipe every owned parser byte and flush the kernel input queue. Work is
    /// bounded; excess late input or OS failure leaves ingress paused.
    /// # Errors
    /// Returns Terminal or Budget without resuming ingress.
    pub fn drain_and_reset(&mut self) -> Result<(), LinuxInputError> {
        self.pause();
        self.decoder.reset();
        self.epoch = self.epoch.checked_add(1).ok_or(LinuxInputError::Budget)?;
        // SAFETY: live owned terminal descriptor; input-only flush.
        if unsafe { libc::tcflush(self.tty.as_raw_fd(), libc::TCIFLUSH) } != 0 {
            return Err(LinuxInputError::Terminal);
        }
        let mut scratch = [0u8; 256];
        for _ in 0..32 {
            let result = self.tty.read(&mut scratch);
            scratch.zeroize();
            match result {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(0) | Err(_) => return Err(LinuxInputError::Terminal),
                Ok(_) => {}
            }
        }
        Err(LinuxInputError::Budget)
    }

    /// Resume only after raw reset AND application epoch drain AND all enclosing
    /// recording/trace owners acknowledge suppression (entry) or restoration
    /// (exit). A local flag alone is not acknowledgment.
    /// # Safety
    /// Caller must establish all these barriers without intervening input reads.
    pub const unsafe fn resume_after_fences(&mut self) {
        self.paused = false;
    }

    /// Poll one event without blocking. Route directly into the private ingress
    /// gate BEFORE tracing, cloning, recording, or normal application admission.
    /// Bare Escape requires an application timeout calling `escape_timeout`.
    /// # Errors
    /// Any malformed/unsupported input or read failure quarantines ingress.
    pub fn next_event(&mut self) -> Result<Option<LinuxInputEvent>, LinuxInputError> {
        if self.paused {
            return Err(LinuxInputError::Paused);
        }
        let result = self.read_next();
        if result.is_err() {
            self.pause();
            self.decoder.reset();
        }
        result.map(|event| {
            event.map(|event| LinuxInputEvent {
                epoch: self.epoch,
                event,
            })
        })
    }
    /// Current ingress epoch; queued older events must be wiped, not admitted.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
    fn read_next(&mut self) -> Result<Option<Event>, LinuxInputError> {
        if let Some(event) = self.decoder.next()? {
            return Ok(Some(event));
        }
        let mut scratch = [0u8; 256];
        let read = self.tty.read(&mut scratch);
        let result = match read {
            Ok(0) => Err(LinuxInputError::Terminal),
            Ok(n) => self
                .decoder
                .push(&scratch[..n])
                .and_then(|()| self.decoder.next()),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(_) => Err(LinuxInputError::Terminal),
        };
        scratch.zeroize();
        result
    }
    /// Resolve only a standalone ESC after a bounded application timeout.
    pub fn escape_timeout(&mut self) -> Option<LinuxInputEvent> {
        if self.paused || self.decoder.len != 1 || self.decoder.bytes[0] != 27 {
            return None;
        }
        self.decoder.reset();
        Some(LinuxInputEvent {
            epoch: self.epoch,
            event: Event::Key(KeyStroke::simple(KeyCode::Escape)),
        })
    }
}
impl Drop for LinuxInputReader {
    fn drop(&mut self) {
        let _ = self.drain_and_reset();
        // SAFETY: restore only this owner's original termios on a live FD.
        unsafe {
            libc::tcsetattr(
                self.tty.as_raw_fd(),
                libc::TCSANOW,
                &raw const self.original,
            );
        }
    }
}

struct Decoder {
    bytes: [u8; CAPACITY],
    len: usize,
}
impl Decoder {
    const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            len: 0,
        }
    }
    fn reset(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }
    fn push(&mut self, bytes: &[u8]) -> Result<(), LinuxInputError> {
        if bytes.len() > CAPACITY - self.len {
            self.reset();
            return Err(LinuxInputError::Budget);
        }
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
        Ok(())
    }
    fn consume(&mut self, n: usize) {
        self.bytes[..n].zeroize();
        self.bytes.copy_within(n..self.len, 0);
        self.bytes[self.len - n..self.len].zeroize();
        self.len -= n;
    }
    fn next(&mut self) -> Result<Option<Event>, LinuxInputError> {
        let bytes = &self.bytes[..self.len];
        if bytes.is_empty() {
            return Ok(None);
        }
        if PASTE_START.starts_with(bytes) {
            return Ok(None);
        }
        if bytes.starts_with(PASTE_START) {
            let body = &bytes[PASTE_START.len()..];
            let Some(end) = body.windows(PASTE_END.len()).position(|w| w == PASTE_END) else {
                return Ok(None);
            };
            let text = std::str::from_utf8(&body[..end])
                .map_err(|_| LinuxInputError::Protocol)?
                .to_owned();
            self.consume(PASTE_START.len() + end + PASTE_END.len());
            return Ok(Some(Event::Paste(text)));
        }
        if bytes[0] == 27 {
            let Some(mut decoded) = legacy::decode_escape(bytes) else {
                return Ok(None);
            };
            decoded.raw.zeroize();
            // The legacy decoder's fallback ESC would admit an unknown CSI tail
            // as text. Reject it instead; never split unsupported protocols.
            if decoded.consumed == 1 {
                return Err(LinuxInputError::Protocol);
            }
            let event = Event::Key(decoded.stroke);
            self.consume(decoded.consumed);
            return Ok(Some(event));
        }
        if bytes[0].is_ascii() {
            let (mut stroke, mut raw) = legacy::decode_single(bytes[0]);
            // TUI Char denotes the produced text, unlike the forwarding
            // decoder's normalized binding notation for shifted ASCII.
            if bytes[0].is_ascii_uppercase() {
                stroke.key = KeyCode::Char(char::from(bytes[0]));
            }
            raw.zeroize();
            self.consume(1);
            return Ok(Some(Event::Key(stroke)));
        }
        let width = match bytes[0] {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(LinuxInputError::Protocol),
        };
        if bytes.len() < width {
            return Ok(None);
        }
        let character = std::str::from_utf8(&bytes[..width])
            .map_err(|_| LinuxInputError::Protocol)?
            .chars()
            .next()
            .ok_or(LinuxInputError::Protocol)?;
        self.consume(width);
        Ok(Some(Event::Key(KeyStroke::simple(KeyCode::Char(
            character,
        )))))
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::sync::Mutex;
    static OWNER_TEST: Mutex<()> = Mutex::new(());

    fn pty() -> (File, LinuxInputReader) {
        let ownership = Ownership::acquire().unwrap();
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid output pointers; no name/termios/winsize requested.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: successful openpty returns two distinct owned descriptors.
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        // SAFETY: live descriptor; test owns the complete PTY lifetime.
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        (
            master,
            LinuxInputReader::from_owned_file(slave, ownership).unwrap(),
        )
    }

    #[test]
    fn pty_canary_and_late_input_do_not_cross_a_reset_epoch() {
        let _serial = OWNER_TEST.lock().unwrap();
        let (mut master, mut reader) = pty();
        reader.drain_and_reset().unwrap();
        // SAFETY: isolated PTY has no recorder or application event queue.
        unsafe {
            reader.resume_after_fences();
        }
        master
            .write_all(b"\x1b[200~synthetic-private-canary")
            .unwrap();
        let mut consumed = false;
        for _ in 0..100 {
            assert!(reader.next_event().unwrap().is_none());
            if reader.decoder.len > 0 {
                consumed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(consumed);
        let prior_epoch = reader.epoch();
        reader.pause();
        master.write_all(b"late-private-canary\x1b[201~").unwrap();
        assert!(matches!(reader.next_event(), Err(LinuxInputError::Paused)));
        reader.drain_and_reset().unwrap();
        assert!(reader.epoch() > prior_epoch);
        assert!(reader.decoder.bytes.iter().all(|b| *b == 0));
        // SAFETY: test queue is empty and no capture owner exists for isolated PTY.
        unsafe {
            reader.resume_after_fences();
        }
        assert!(reader.next_event().unwrap().is_none());
        master.write_all(b"\x1b[999u").unwrap();
        let mut failed = false;
        for _ in 0..100 {
            match reader.next_event() {
                Err(LinuxInputError::Protocol) => {
                    failed = true;
                    break;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(1)),
                _ => panic!("unsupported protocol was admitted"),
            }
        }
        assert!(failed);
        assert!(matches!(reader.next_event(), Err(LinuxInputError::Paused)));
        assert!(reader.decoder.bytes.iter().all(|b| *b == 0));
    }
    #[test]
    fn stale_envelopes_are_not_admitted_and_pending_payloads_wipe() {
        let stale = LinuxInputEvent {
            epoch: 4,
            event: Event::Paste("synthetic-stale-canary".into()),
        };
        assert!(stale.take_for_epoch(5).is_none());
        let current = LinuxInputEvent {
            epoch: 5,
            event: Event::Key(KeyStroke::simple(KeyCode::Enter)),
        };
        assert_eq!(
            current.take_for_epoch(5),
            Some(Event::Key(KeyStroke::simple(KeyCode::Enter)))
        );
        // Test the same erasure operation used by Drop while storage is visible.
        let mut pending = LinuxInputEvent {
            epoch: 5,
            event: Event::Paste("synthetic-private-canary".into()),
        };
        pending.wipe();
        assert!(matches!(&pending.event, Event::Paste(text) if text.is_empty()));
        pending.event = Event::Tick;
    }

    #[test]
    fn fragmented_utf8_csi_and_paste_share_owned_storage() {
        let mut decoder = Decoder::new();
        decoder.push(&[0xf0, 0x9f]).unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder.push(&[0x98, 0x80]).unwrap();
        assert_eq!(
            decoder.next().unwrap(),
            Some(Event::Key(KeyStroke::simple(KeyCode::Char('😀'))))
        );
        decoder.push(b"\x1b[").unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder.push(b"A").unwrap();
        assert_eq!(
            decoder.next().unwrap(),
            Some(Event::Key(KeyStroke::simple(KeyCode::Up)))
        );
        decoder.push(b"\x1b[200~synthetic-canary\x1b[20").unwrap();
        assert!(decoder.next().unwrap().is_none());
        decoder.push(b"1~").unwrap();
        assert_eq!(
            decoder.next().unwrap(),
            Some(Event::Paste("synthetic-canary".into()))
        );
        assert!(decoder.bytes.iter().all(|b| *b == 0));
    }
    #[test]
    fn reset_erases_incomplete_input_and_excess_is_bounded() {
        let mut decoder = Decoder::new();
        decoder.push(b"\x1b[200~synthetic-canary").unwrap();
        decoder.reset();
        assert!(decoder.next().unwrap().is_none());
        assert!(decoder.bytes.iter().all(|b| *b == 0));
        assert_eq!(
            decoder.push(&[b'x'; CAPACITY + 1]),
            Err(LinuxInputError::Budget)
        );
        assert_eq!(decoder.len, 0);
        decoder.push(b"\x1b[999u").unwrap();
        assert_eq!(decoder.next(), Err(LinuxInputError::Protocol));
    }
    #[test]
    fn owner_is_exclusive_until_dropped() {
        let _serial = OWNER_TEST.lock().unwrap();
        let owner = Ownership::acquire().unwrap();
        assert!(matches!(Ownership::acquire(), Err(LinuxInputError::Busy)));
        drop(owner);
        assert!(Ownership::acquire().is_ok());
    }
}
