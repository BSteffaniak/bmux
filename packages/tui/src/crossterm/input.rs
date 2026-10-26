//! Single-owner Linux terminal packets, preserving opaque protocol input.

use crate::{
    event::{Event, FocusEvent, MouseButton, MouseEvent, MouseEventKind, MouseModifiers},
    geometry::{Point, Size},
};
use bmux_keyboard::{KeyCode, KeyStroke};
use std::{
    io::{self, Read},
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 64 * 1024;
const MAX_FRAME: usize = 4096;
const ESC_WAIT: Duration = Duration::from_millis(30);

/// Lossless Linux input packet. Opaque bytes are never interpreted as application text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxInputPacket {
    /// Decoded ordinary terminal input.
    Event(Event),
    /// A repeat or release, retaining metadata absent from the TUI action model.
    Key {
        /// Decoded key and modifiers.
        stroke: KeyStroke,
        /// Protocol event kind.
        kind: bmux_keyboard::csi_u::KeyEventKind,
    },
    /// Unmatched protocol frame or undecodable bytes, unchanged.
    Opaque(Vec<u8>),
}

impl LinuxInputPacket {
    /// Convert to a TUI action: repeats type, releases do not. Opaque input is wiped.
    #[must_use]
    pub fn into_event(self) -> Option<Event> {
        match self {
            Self::Event(event) => Some(event),
            Self::Key {
                kind: bmux_keyboard::csi_u::KeyEventKind::Release,
                ..
            } => None,
            Self::Key { stroke, .. } => Some(Event::Key(stroke)),
            Self::Opaque(mut bytes) => {
                use zeroize::Zeroize;
                bytes.zeroize();
                None
            }
        }
    }

    /// Convert to an attach event without losing press/repeat/release identity.
    #[must_use]
    pub fn into_crossterm(self) -> Option<crossterm::event::Event> {
        match self {
            Self::Key { stroke, kind } => {
                let crossterm::event::Event::Key(mut key) =
                    super::event_to_crossterm(Event::Key(stroke))?
                else {
                    return None;
                };
                key.kind = match kind {
                    bmux_keyboard::csi_u::KeyEventKind::Press => {
                        crossterm::event::KeyEventKind::Press
                    }
                    bmux_keyboard::csi_u::KeyEventKind::Repeat => {
                        crossterm::event::KeyEventKind::Repeat
                    }
                    bmux_keyboard::csi_u::KeyEventKind::Release => {
                        crossterm::event::KeyEventKind::Release
                    }
                };
                Some(crossterm::event::Event::Key(key))
            }
            other => other.into_event().and_then(super::event_to_crossterm),
        }
    }
}

struct Decoder {
    pending: Vec<u8>,
    since: Option<Instant>,
    failed: bool,
}
impl Default for Decoder {
    fn default() -> Self {
        // Fixed capacity avoids reallocating un-wiped private prefixes while a
        // fragmented key/paste frame is being assembled.
        Self {
            pending: Vec::with_capacity(MAX_PENDING),
            since: None,
            failed: false,
        }
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.pending.zeroize();
    }
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8], now: Instant) -> io::Result<()> {
        if self.failed || self.pending.len().saturating_add(bytes.len()) > MAX_PENDING {
            self.failed = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal input budget exceeded",
            ));
        }
        if self.pending.is_empty() {
            self.since = Some(now);
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }
    #[expect(
        clippy::unnecessary_wraps,
        reason = "uniform packet extraction helpers compose with incomplete-input branches"
    )]
    fn take(
        &mut self,
        count: usize,
        packet: LinuxInputPacket,
        now: Instant,
    ) -> Option<LinuxInputPacket> {
        use zeroize::Zeroize;
        let len = self.pending.len();
        self.pending.copy_within(count.., 0);
        self.pending[len - count..].zeroize();
        self.pending.truncate(len - count);
        self.since = (!self.pending.is_empty()).then_some(now);
        Some(packet)
    }
    fn opaque(&mut self, count: usize, now: Instant) -> Option<LinuxInputPacket> {
        let bytes = self.pending[..count].to_vec();
        self.take(count, LinuxInputPacket::Opaque(bytes), now)
    }
    #[expect(
        clippy::too_many_lines,
        reason = "ordered bounded wire framing keeps consumption decisions adjacent; no side effects before complete packets"
    )]
    fn next(&mut self, now: Instant) -> io::Result<Option<LinuxInputPacket>> {
        if self.failed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "terminal input failed closed",
            ));
        }
        if self.pending.is_empty() {
            return Ok(None);
        }
        let bytes = &self.pending;
        let expired = self
            .since
            .is_some_and(|since| now.saturating_duration_since(since) >= ESC_WAIT);
        // Never reinterpret a delayed recognized paste opener as executable keys.
        if bytes.len() > 1 && b"\x1b[200~".starts_with(bytes) {
            return Ok(None);
        }
        if bytes.starts_with(b"\x1b[200~") {
            if let Some(end) = bytes[6..].windows(6).position(|part| part == b"\x1b[201~") {
                let count = end + 12;
                let packet = std::str::from_utf8(&bytes[6..end + 6]).map_or_else(
                    |_| LinuxInputPacket::Opaque(bytes[..count].to_vec()),
                    |text| LinuxInputPacket::Event(Event::Paste(text.to_owned())),
                );
                return Ok(self.take(count, packet, now));
            }
            return Ok(None);
        }
        if bytes.starts_with(b"\x1b_")
            || bytes.starts_with(b"\x1bP")
            || bytes.starts_with(b"\x1b]")
            || bytes.starts_with(b"\x1b^")
        {
            let st = bytes[2..]
                .windows(2)
                .position(|part| part == b"\x1b\\")
                .map(|end| end + 4);
            let bel = bytes
                .starts_with(b"\x1b]")
                .then(|| {
                    bytes[2..]
                        .iter()
                        .position(|byte| *byte == 7)
                        .map(|end| end + 3)
                })
                .flatten();
            let end = match (st, bel) {
                (Some(st), Some(bel)) => Some(st.min(bel)),
                (st, bel) => st.or(bel),
            };
            if let Some(end) = end {
                if end > MAX_FRAME {
                    self.failed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "terminal control frame budget exceeded",
                    ));
                }
                return Ok(self.opaque(end, now));
            }
            if bytes.len() >= MAX_FRAME {
                self.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "terminal control frame budget exceeded",
                ));
            }
            return Ok(None);
        }
        if bytes[0] == 0x1b {
            if bytes.len() == 1 {
                return Ok(if expired {
                    self.take(
                        1,
                        LinuxInputPacket::Event(Event::Key(KeyStroke::simple(KeyCode::Escape))),
                        now,
                    )
                } else {
                    None
                });
            }
            if bytes[1] == b'[' || bytes[1] == b'O' {
                if bytes.starts_with(b"\x1b[M") {
                    if bytes.len() < 6 {
                        return Ok(None);
                    }
                    let values = (
                        bytes[3].checked_sub(32),
                        bytes[4].checked_sub(32),
                        bytes[5].checked_sub(32),
                    );
                    if let (Some(button), Some(x), Some(y)) = values {
                        let event = mouse(
                            u32::from(button),
                            u32::from(x),
                            u32::from(y),
                            button & 3 == 3,
                        );
                        if let Some(event) = event {
                            return Ok(self.take(
                                6,
                                LinuxInputPacket::Event(Event::Mouse(event)),
                                now,
                            ));
                        }
                    }
                    return Ok(self.opaque(6, now));
                }
                let Some(end) = bytes[2..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .map(|index| index + 3)
                else {
                    if bytes.len() >= MAX_FRAME {
                        self.failed = true;
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "CSI budget exceeded",
                        ));
                    }
                    return Ok(if expired {
                        self.opaque(bytes.len(), now)
                    } else {
                        None
                    });
                };
                let sequence = &bytes[..end];
                if sequence == b"\x1b[I" || sequence == b"\x1b[O" {
                    let event = if sequence[2] == b'I' {
                        FocusEvent::Gained
                    } else {
                        FocusEvent::Lost
                    };
                    return Ok(self.take(end, LinuxInputPacket::Event(Event::Focus(event)), now));
                }
                if sequence.starts_with(b"\x1b[<") && matches!(sequence.last(), Some(b'M' | b'm')) {
                    if let Ok(params) = std::str::from_utf8(&sequence[3..end - 1]) {
                        let mut values = params.split(';').map(str::parse::<u32>);
                        if let (Some(Ok(button)), Some(Ok(x)), Some(Ok(y)), None) =
                            (values.next(), values.next(), values.next(), values.next())
                            && let Some(event) = mouse(button, x, y, sequence[end - 1] == b'm')
                        {
                            return Ok(self.take(
                                end,
                                LinuxInputPacket::Event(Event::Mouse(event)),
                                now,
                            ));
                        }
                    }
                    return Ok(self.opaque(end, now));
                }
                let decoded = bmux_keyboard::csi_u::decode(sequence)
                    .map(|result| (result.stroke, result.consumed, result.kind))
                    .or_else(|| {
                        bmux_keyboard::legacy::decode_escape(sequence).map(|result| {
                            (
                                result.stroke,
                                result.consumed,
                                bmux_keyboard::csi_u::KeyEventKind::Press,
                            )
                        })
                    });
                if let Some((key, consumed, kind)) = decoded
                    && consumed == end
                    && key.key != KeyCode::Escape
                {
                    let packet = if kind == bmux_keyboard::csi_u::KeyEventKind::Press {
                        LinuxInputPacket::Event(Event::Key(key))
                    } else {
                        LinuxInputPacket::Key { stroke: key, kind }
                    };
                    return Ok(self.take(end, packet, now));
                }
                return Ok(self.opaque(end, now));
            }
            // Alt prefixes a complete UTF-8 character/control, never just one byte.
            if let Some((mut key, consumed)) = character(&bytes[1..]) {
                key.modifiers.alt = true;
                return Ok(self.take(consumed + 1, LinuxInputPacket::Event(Event::Key(key)), now));
            }
            return Ok(if expired {
                self.opaque(bytes.len(), now)
            } else {
                None
            });
        }
        if let Some((key, consumed)) = character(bytes) {
            return Ok(self.take(consumed, LinuxInputPacket::Event(Event::Key(key)), now));
        }
        if std::str::from_utf8(bytes).is_err_and(|error| error.error_len().is_some()) {
            return Ok(self.opaque(1, now));
        }
        Ok(None)
    }
}

fn character(bytes: &[u8]) -> Option<(KeyStroke, usize)> {
    let first = *bytes.first()?;
    if first < 128 {
        return Some((bmux_keyboard::legacy::decode_single(first).0, 1));
    }
    let width = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let text = std::str::from_utf8(bytes.get(..width)?).ok()?;
    Some((
        KeyStroke::simple(KeyCode::Char(text.chars().next()?)),
        width,
    ))
}

fn mouse(button: u32, x: u32, y: u32, release: bool) -> Option<MouseEvent> {
    let position = Point::new(
        u16::try_from(x.checked_sub(1)?).ok()?,
        u16::try_from(y.checked_sub(1)?).ok()?,
    );
    let which = match button & 3 {
        0 => MouseButton::Left,
        1 => MouseButton::Middle,
        2 => MouseButton::Right,
        _ => MouseButton::Other(3),
    };
    let kind = if button & 64 != 0 {
        match button & 3 {
            0 => MouseEventKind::ScrollUp,
            1 => MouseEventKind::ScrollDown,
            2 => MouseEventKind::ScrollLeft,
            _ => MouseEventKind::ScrollRight,
        }
    } else if release {
        MouseEventKind::Up(which)
    } else if button & 32 != 0 {
        if button & 3 == 3 {
            MouseEventKind::Move
        } else {
            MouseEventKind::Drag(which)
        }
    } else {
        MouseEventKind::Down(which)
    };
    Some(
        MouseEvent::new(kind, position).with_modifiers(MouseModifiers {
            shift: button & 4 != 0,
            alt: button & 8 != 0,
            ctrl: button & 16 != 0,
        }),
    )
}

struct RawOwnership;
impl RawOwnership {
    fn acquire() -> io::Result<Self> {
        if !super::claim_raw_input() {
            return Err(io::Error::other("terminal input ownership unavailable"));
        }
        Ok(Self)
    }
}
impl Drop for RawOwnership {
    fn drop(&mut self) {
        super::release_raw_input();
    }
}

/// Sole nonblocking Linux raw input owner. Do not combine with Crossterm readers.
pub struct RawInput {
    file: std::fs::File,
    decoder: Decoder,
    size: Option<Size>,
    // Release only after the descriptor and decoder have been dropped.
    _ownership: RawOwnership,
}
impl RawInput {
    /// Adopt an exclusively owned nonblocking raw tty reader.
    /// Caller configures raw mode and must not create competing readers.
    /// # Errors
    /// Returns `Unsupported` when the descriptor is not a nonblocking tty.
    pub fn from_owned_tty(file: std::fs::File) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let ownership = RawOwnership::acquire()?;
        // SAFETY: descriptor remains live and owned throughout inspection.
        if unsafe { libc::isatty(file.as_raw_fd()) } != 1
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "raw reader requires nonblocking tty",
            ));
        }
        Ok(Self {
            _ownership: ownership,
            file,
            decoder: Decoder::default(),
            size: None,
        })
    }

    /// Open the actual stdout tty for independent nonblocking reads.
    /// # Errors
    /// Returns tty identity/open errors; redirected stdout is unsupported.
    pub fn open_stdout_tty() -> io::Result<Self> {
        use std::os::{
            fd::AsRawFd,
            unix::fs::{MetadataExt, OpenOptionsExt},
        };
        let ownership = RawOwnership::acquire()?;
        // SAFETY: isatty only inspects stdout.
        if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stdout is not tty",
            ));
        }
        let identity = std::fs::metadata("/proc/self/fd/1")?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/proc/self/fd/1")?;
        let actual = file.metadata()?;
        if (identity.dev(), identity.ino(), identity.rdev())
            != (actual.dev(), actual.ino(), actual.rdev())
        {
            return Err(io::Error::other("tty identity mismatch"));
        }
        // SAFETY: owned live tty descriptor.
        if unsafe { libc::isatty(file.as_raw_fd()) } != 1 {
            return Err(io::Error::other("not tty"));
        }
        Ok(Self {
            _ownership: ownership,
            file,
            decoder: Decoder::default(),
            size: None,
        })
    }
    /// Wipe buffered decoded input and flush pending raw tty input at an admission barrier.
    /// The caller must serialize this with reads and fence all application-pending events.
    /// # Errors
    /// Returns a terminal flush failure, leaving the caller responsible for quarantine.
    pub fn drain_and_reset(&mut self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        use zeroize::Zeroize;
        self.decoder.pending.zeroize();
        self.decoder = Decoder::default();
        // SAFETY: the descriptor is live and exclusively owned by this reader.
        if unsafe { libc::tcflush(self.file.as_raw_fd(), libc::TCIFLUSH) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read or decode at most one bounded packet. Caller must retry after ESC deadline.
    /// # Errors
    /// Returns read/ioctl/budget errors; no bytes are silently truncated.
    pub fn read_ready(&mut self, now: Instant) -> io::Result<Option<LinuxInputPacket>> {
        use std::os::fd::AsRawFd;
        if let Some(packet) = self.decoder.next(now)? {
            return Ok(Some(packet));
        }
        let mut bytes = zeroize::Zeroizing::new([0u8; 4096]);
        match self.file.read(&mut bytes[..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal closed",
                ));
            }
            Ok(count) => {
                self.decoder.feed(&bytes[..count], now)?;
                return self.decoder.next(now);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        // SAFETY: live descriptor and correctly sized output storage.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful ioctl initialized winsize.
        let size = unsafe { size.assume_init() };
        let size = Size::new(size.ws_col, size.ws_row);
        if self.size.replace(size) != Some(size) {
            return Ok(Some(LinuxInputPacket::Event(Event::Resize(size))));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn fragmented_key_kinds_survive_attach_and_filter_tui_actions() {
        use crossterm::event::{Event as CEvent, KeyEventKind as CKind};
        let now = Instant::now();
        for (code, final_byte) in [("97", 'u'), ("1", 'A'), ("3", '~')] {
            for (suffix, expected) in [
                (":1", CKind::Press),
                (":2", CKind::Repeat),
                (":3", CKind::Release),
            ] {
                let sequence = format!("\x1b[{code};5{suffix}{final_byte}");
                for split in 1..sequence.len() {
                    let mut decoder = Decoder::default();
                    decoder.feed(&sequence.as_bytes()[..split], now).unwrap();
                    assert!(decoder.next(now).unwrap().is_none());
                    decoder.feed(&sequence.as_bytes()[split..], now).unwrap();
                    let packet = decoder.next(now).unwrap().unwrap();
                    let Some(CEvent::Key(key)) = packet.clone().into_crossterm() else {
                        panic!("missing key");
                    };
                    assert_eq!(key.kind, expected);
                    assert!(
                        key.modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL)
                    );
                    assert_eq!(packet.into_event().is_some(), expected != CKind::Release);
                    assert!(decoder.next(now).unwrap().is_none());
                }
            }
        }
        for (sequence, count) in [
            (b"\x1b[97;1:1u\x1b[97;1:3u".as_slice(), 1),
            (
                b"\x1b[97;1:1u\x1b[97;1:2u\x1b[97;1:2u\x1b[97;1:3u".as_slice(),
                3,
            ),
        ] {
            let mut decoder = Decoder::default();
            decoder.feed(sequence, now).unwrap();
            let mut typed = 0;
            while let Some(packet) = decoder.next(now).unwrap() {
                if let Some(CEvent::Key(key)) = packet.into_crossterm()
                    && key.kind != CKind::Release
                {
                    typed += 1;
                }
            }
            assert_eq!(typed, count);
        }
    }

    #[test]
    fn colon_keys_delayed_paste_and_bel_osc_regressions() {
        let now = Instant::now();
        for sequence in [b"\x1b[97;5:1u".as_slice(), b"\x1b[97:65;2u".as_slice()] {
            let mut decoder = Decoder::default();
            decoder.feed(sequence, now).unwrap();
            assert!(matches!(
                decoder.next(now).unwrap(),
                Some(LinuxInputPacket::Event(Event::Key(_)))
            ));
        }
        let mut decoder = Decoder::default();
        decoder.feed(b"\x1b[20", now).unwrap();
        assert!(
            decoder
                .next(now + Duration::from_secs(1))
                .unwrap()
                .is_none()
        );
        decoder
            .feed(b"0~hello\x1b[201~", now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            decoder.next(now + Duration::from_secs(1)).unwrap(),
            Some(LinuxInputPacket::Event(Event::Paste("hello".into())))
        );
        let mut decoder = Decoder::default();
        let mut bytes = b"\x1b]".to_vec();
        bytes.extend(vec![b'x'; MAX_FRAME]);
        bytes.push(7);
        decoder.feed(&bytes, now).unwrap();
        assert!(decoder.next(now).is_err());
    }

    #[test]
    fn modified_keys_utf8_alt_and_fragment_boundaries() {
        let now = Instant::now();
        let cases: &[&[u8]] = &[b"\x1b[97;5u", b"\x1b[1;2A", "\u{1b}é".as_bytes()];
        for sequence in cases {
            for split in 1..sequence.len() {
                let mut decoder = Decoder::default();
                decoder.feed(&sequence[..split], now).unwrap();
                assert!(
                    decoder.next(now).unwrap().is_none(),
                    "premature key at split {split}"
                );
                decoder.feed(&sequence[split..], now).unwrap();
                assert!(matches!(
                    decoder.next(now).unwrap(),
                    Some(LinuxInputPacket::Event(Event::Key(_)))
                ));
                assert!(decoder.next(now).unwrap().is_none());
            }
        }
    }

    #[test]
    fn oversized_terminated_control_frame_fails_closed() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        let mut bytes = b"\x1b_".to_vec();
        bytes.extend(vec![b'x'; MAX_FRAME]);
        bytes.extend_from_slice(b"\x1b\\");
        decoder.feed(&bytes, now).unwrap();
        assert!(decoder.next(now).is_err());
        assert!(decoder.next(now).is_err());
    }

    fn assert_pty_key_kinds(master: &mut std::fs::File, reader: &mut RawInput) {
        use crossterm::event::{Event as CEvent, KeyCode as CCode, KeyEventKind as Kind};
        use std::io::Write;
        master
            .write_all(b"\x1b[97;1:1u\x1b[97;1:2u\x1b[97;1:3u")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut kinds = Vec::new();
        while kinds.len() < 3 {
            if let Some(packet) = reader.read_ready(Instant::now()).unwrap()
                && let Some(CEvent::Key(key)) = packet.into_crossterm()
            {
                assert_eq!(key.code, CCode::Char('a'));
                kinds.push(key.kind);
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(kinds, vec![Kind::Press, Kind::Repeat, Kind::Release]);
    }

    #[test]
    fn real_pty_fragmented_packets_resize_and_flags() {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: valid descriptor outputs with optional arguments omitted.
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
        // SAFETY: successful openpty yields distinct owned file descriptors.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        // SAFETY: owned slave descriptor from openpty.
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: output points to correctly sized termios.
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), attrs.as_mut_ptr()) },
            0
        );
        // SAFETY: tcgetattr succeeded.
        let mut attrs = unsafe { attrs.assume_init() };
        // SAFETY: initialized local termios; live exclusive test tty.
        unsafe {
            libc::cfmakeraw(&raw mut attrs);
            assert_eq!(
                libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const attrs),
                0
            );
        }
        // SAFETY: exclusively owned tty descriptor.
        assert_eq!(
            unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
            0
        );
        let mut reader = RawInput::from_owned_tty(slave).unwrap();
        assert!(RawOwnership::acquire().is_err());
        assert!(super::super::read_event().is_err());
        master.write_all(b"\x1b_Gi=42;O").unwrap();
        for _ in 0..10 {
            assert!(!matches!(
                reader.read_ready(Instant::now()).unwrap(),
                Some(LinuxInputPacket::Opaque(_))
            ));
        }
        master.write_all(b"K\x1b\\q").unwrap();
        let mut packets = Vec::new();
        for _ in 0..100 {
            if let Some(packet) = reader.read_ready(Instant::now()).unwrap()
                && !matches!(packet, LinuxInputPacket::Event(Event::Resize(_)))
            {
                packets.push(packet);
            }
            if packets.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            packets[0],
            LinuxInputPacket::Opaque(b"\x1b_Gi=42;OK\x1b\\".to_vec())
        );
        assert!(
            matches!(&packets[1], LinuxInputPacket::Event(Event::Key(key)) if key.key == KeyCode::Char('q'))
        );
        // Incomplete decoded paste and kernel-pending bytes are both erased by a
        // real PTY barrier. The first fresh key is not appended to either canary.
        master
            .write_all(b"\x1b[200~decoded-private-canary")
            .unwrap();
        for _ in 0..10 {
            let _ = reader.read_ready(Instant::now()).unwrap();
        }
        master.write_all(b"kernel-private-canary").unwrap();
        reader.drain_and_reset().unwrap();
        assert!(reader.decoder.pending.is_empty());
        master.write_all(b"z").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(LinuxInputPacket::Event(Event::Key(key))) =
                reader.read_ready(Instant::now()).unwrap()
            {
                assert_eq!(key.key, KeyCode::Char('z'));
                break;
            }
            assert!(Instant::now() < deadline);
        }
        assert_pty_key_kinds(&mut master, &mut reader);
        let size = libc::winsize {
            ws_row: 41,
            ws_col: 93,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: correctly sized initialized winsize and live master descriptor.
        assert_eq!(
            unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) },
            0
        );
        assert_eq!(
            reader.read_ready(Instant::now()).unwrap(),
            Some(LinuxInputPacket::Event(Event::Resize(Size::new(93, 41))))
        );
    }

    #[test]
    fn unicode_control_mouse_focus_and_budgets() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        decoder
            .feed("é\u{1}\u{1b}[<65;4;3M\u{1b}[I".as_bytes(), now)
            .unwrap();
        assert!(
            matches!(decoder.next(now).unwrap(), Some(LinuxInputPacket::Event(Event::Key(key))) if key.key == KeyCode::Char('é'))
        );
        assert!(
            matches!(decoder.next(now).unwrap(), Some(LinuxInputPacket::Event(Event::Key(key))) if key.modifiers.ctrl)
        );
        assert!(
            matches!(decoder.next(now).unwrap(), Some(LinuxInputPacket::Event(Event::Mouse(event))) if event.kind == MouseEventKind::ScrollDown && event.position == Point::new(3, 2))
        );
        assert_eq!(
            decoder.next(now).unwrap(),
            Some(LinuxInputPacket::Event(Event::Focus(FocusEvent::Gained)))
        );
        let mut decoder = Decoder::default();
        assert!(decoder.feed(&vec![0; MAX_PENDING + 1], now).is_err());
        assert!(decoder.feed(b"q", now).is_err());
        assert!(decoder.next(now).is_err());
    }

    #[test]
    fn osc_earliest_terminator_preserves_following_ack_at_every_split() {
        let now = Instant::now();
        let osc = b"\x1b]0;title\x07";
        let ack = b"\x1b_Gi=42;OK\x1b\\";
        let wire = [osc.as_slice(), ack.as_slice()].concat();
        for split in 0..=wire.len() {
            let mut decoder = Decoder::default();
            let mut packets = Vec::new();
            for part in [&wire[..split], &wire[split..]] {
                decoder.feed(part, now).unwrap();
                while let Some(packet) = decoder.next(now).unwrap() {
                    packets.push(packet);
                }
            }
            assert_eq!(
                packets,
                vec![
                    LinuxInputPacket::Opaque(osc.to_vec()),
                    LinuxInputPacket::Opaque(ack.to_vec())
                ],
                "split {split}"
            );
        }
        let mut decoder = Decoder::default();
        let apc = b"\x1b_Gi=42;O\x07K\x1b\\";
        decoder.feed(apc, now).unwrap();
        assert_eq!(
            decoder.next(now).unwrap(),
            Some(LinuxInputPacket::Opaque(apc.to_vec()))
        );
    }

    #[test]
    fn fragmented_apc_and_key_are_lossless() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        decoder.feed(b"\x1b_Gi=7;O", now).unwrap();
        assert!(decoder.next(now).unwrap().is_none());
        decoder.feed(b"K\x1b\\q", now).unwrap();
        assert_eq!(
            decoder.next(now).unwrap(),
            Some(LinuxInputPacket::Opaque(b"\x1b_Gi=7;OK\x1b\\".to_vec()))
        );
        assert!(
            matches!(decoder.next(now).unwrap(), Some(LinuxInputPacket::Event(Event::Key(key))) if key.key == bmux_keyboard::KeyCode::Char('q'))
        );
    }

    #[test]
    fn paste_shields_escapes_and_escape_has_deadline() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        decoder
            .feed(b"\x1b[200~hello\x1b_Gfake\x1b\\\x1b[201~\x1b", now)
            .unwrap();
        assert_eq!(
            decoder.next(now).unwrap(),
            Some(LinuxInputPacket::Event(Event::Paste(
                "hello\x1b_Gfake\x1b\\".into()
            )))
        );
        assert!(decoder.next(now).unwrap().is_none());
        assert!(
            matches!(decoder.next(now + Duration::from_millis(40)).unwrap(), Some(LinuxInputPacket::Event(Event::Key(key))) if key.key == bmux_keyboard::KeyCode::Escape)
        );
    }

    #[test]
    fn invalid_utf8_and_unknown_sequences_are_opaque_not_dropped() {
        let now = Instant::now();
        let mut decoder = Decoder::default();
        decoder.feed(b"\xff\x1b[999z", now).unwrap();
        assert_eq!(
            decoder.next(now).unwrap(),
            Some(LinuxInputPacket::Opaque(vec![255]))
        );
        assert_eq!(
            decoder.next(now).unwrap(),
            Some(LinuxInputPacket::Opaque(b"\x1b[999z".to_vec()))
        );
    }
}
