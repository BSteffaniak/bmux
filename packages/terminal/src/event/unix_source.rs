//! Unix terminal input source.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::reader::{InternalEvent, Source, Waker};
use super::unix_parse::Parser;
use super::Event;
use crate::sys::unix::{TtyFd, raw_mode_enabled};
use crate::unix::sigwinch;

const READ_BUFFER_SIZE: usize = 1024;

/// Reads the controlling terminal, resize notifications, and wakeups.
pub struct UnixSource {
    tty: TtyFd,
    parser: Parser,
    buffer: [u8; READ_BUFFER_SIZE],
    resize: UnixStream,
    _resize_registration: sigwinch::Registration,
    wake_receiver: UnixStream,
    wake_sender: Arc<UnixStream>,
}

impl UnixSource {
    pub fn new() -> io::Result<Self> {
        let tty = TtyFd::open()?;
        let (resize, resize_sender) = UnixStream::pair()?;
        resize.set_nonblocking(true)?;
        resize_sender.set_nonblocking(true)?;
        let registration = sigwinch::register(OwnedFd::from(resize_sender))?;
        let (wake_receiver, wake_sender) = UnixStream::pair()?;
        wake_receiver.set_nonblocking(true)?;
        wake_sender.set_nonblocking(true)?;
        Ok(Self {
            tty,
            parser: Parser::default(),
            buffer: [0; READ_BUFFER_SIZE],
            resize,
            _resize_registration: registration,
            wake_receiver,
            wake_sender: Arc::new(wake_sender),
        })
    }

    /// Read whatever the terminal has, without blocking past readiness.
    fn read_tty(&mut self) -> io::Result<()> {
        loop {
            // SAFETY: the buffer is valid for its full length.
            let count = unsafe {
                libc::read(
                    self.tty.raw_fd(),
                    self.buffer.as_mut_ptr().cast(),
                    READ_BUFFER_SIZE,
                )
            };
            if count < 0 {
                let error = io::Error::last_os_error();
                return match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => Ok(()),
                    _ => Err(error),
                };
            }
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal input closed",
                ));
            }
            // `count` is positive and at most the buffer length.
            #[allow(clippy::cast_sign_loss)]
            let count = count as usize;
            self.parser.advance(
                &self.buffer[..count],
                count == READ_BUFFER_SIZE,
                raw_mode_enabled(),
            );
            return Ok(());
        }
    }
}

fn drain(stream: &UnixStream) {
    let mut bytes = [0u8; 64];
    while matches!((&*stream).read(&mut bytes), Ok(count) if count > 0) {}
}

fn poll_timeout(deadline: Option<Instant>) -> libc::c_int {
    deadline.map_or(-1, |deadline| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Round up so a wait never ends just before its deadline.
        let millis = remaining.as_micros().div_ceil(1000);
        libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX)
    })
}

impl Source for UnixSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        if let Some(event) = self.parser.next() {
            return Ok(Some(event));
        }
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: self.tty.raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.resize.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.wake_receiver.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: every descriptor stays open for the call and `fds`
            // holds exactly the entries passed.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 3, poll_timeout(deadline)) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                return Ok(None);
            }
            if fds[0].revents != 0 {
                if fds[0].revents & libc::POLLNVAL != 0 {
                    return Err(io::Error::other("terminal input descriptor is invalid"));
                }
                self.read_tty()?;
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
            }
            if fds[1].revents != 0 {
                drain(&self.resize);
                let (columns, rows) = crate::terminal::size()?;
                return Ok(Some(InternalEvent::Event(Event::Resize(columns, rows))));
            }
            if fds[2].revents != 0 {
                drain(&self.wake_receiver);
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "terminal input wait was woken",
                ));
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
        }
    }

    fn waker(&self) -> Waker {
        let sender = Arc::clone(&self.wake_sender);
        Waker::new(move || {
            use std::io::Write;
            // A full buffer already holds a pending wake.
            let _ = (&*sender).write(&[1]);
        })
    }
}
