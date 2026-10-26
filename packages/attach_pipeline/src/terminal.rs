//! Shared outer-terminal ownership and buffered frame delivery.
//!
//! Input encoding, execution authority and application commands belong to the
//! consumer. Only a successfully written and flushed frame may be committed.

use std::io::{self, Write};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, EventStream, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::queue;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;

/// Modes owned by one frontend. Keyboard enhancement is opt-in and negotiated
/// with the outer terminal after entering raw mode, as in ordinary attach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // Independent terminal modes are acquired and restored individually.
pub struct TerminalOptions {
    pub keyboard_enhanced: bool,
    pub mouse_capture: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub alternate_screen: bool,
    pub disable_wrap: bool,
}

/// An exclusively owned terminal input backend with a real decoder/raw barrier.
///
/// Implementations must not wrap Crossterm's global input reader: dropping its
/// event stream does not discard the global decoder or platform read thread.
pub trait FencedTerminalInput: Send {
    /// Poll one event without blocking or recording its contents.
    /// # Errors
    /// Returns terminal or decoding failures, without private input in diagnostics.
    fn poll_event(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<Option<Event>>>;
    /// Pause reads, wipe decoded/pending events and flush raw input. Remain paused
    /// on failure. The caller serializes its own application queue with this call.
    /// # Errors
    /// Returns failure when the drain/reset proof cannot be established.
    fn drain_and_reset(&mut self) -> io::Result<()>;
    /// Resume only after the application has fenced all prior-generation work.
    /// # Errors
    /// Returns failure when the backend cannot resume the fenced generation.
    fn resume(&mut self) -> io::Result<()>;
}

#[cfg(target_os = "linux")]
struct RawFencedInput {
    reader: bmux_tui::crossterm::input::RawInput,
    paused: bool,
    wake: std::pin::Pin<Box<tokio::time::Sleep>>,
}
#[cfg(target_os = "linux")]
impl FencedTerminalInput for RawFencedInput {
    fn poll_event(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<Option<Event>>> {
        use std::future::Future;
        if self.paused {
            return std::task::Poll::Pending;
        }
        loop {
            match self.reader.read_ready(std::time::Instant::now()) {
                Ok(Some(packet)) => {
                    if let Some(event) = packet.into_crossterm() {
                        return std::task::Poll::Ready(Ok(Some(event)));
                    }
                }
                Err(error) => {
                    self.paused = true;
                    return std::task::Poll::Ready(Err(error));
                }
                Ok(None) => {
                    if self.wake.as_mut().poll(cx).is_pending() {
                        return std::task::Poll::Pending;
                    }
                    self.wake
                        .as_mut()
                        .reset(tokio::time::Instant::now() + std::time::Duration::from_millis(2));
                }
            }
        }
    }
    fn drain_and_reset(&mut self) -> io::Result<()> {
        self.paused = true;
        self.reader.drain_and_reset()
    }
    fn resume(&mut self) -> io::Result<()> {
        self.paused = false;
        Ok(())
    }
}

/// Async input and output share one explicit outer-terminal lifetime.
pub struct TerminalIo<W: Write> {
    writer: W,
    events: Option<EventStream>,
    fenced_input: Option<Box<dyn FencedTerminalInput>>,
    input_generation: u64,
    input_paused: bool,
    owned: Option<TerminalOptions>,
    raw_owned: bool,
}

impl<W: Write> TerminalIo<W> {
    #[must_use]
    pub const fn new(writer: W) -> Self {
        Self {
            writer,
            events: None,
            fenced_input: None,
            input_generation: 0,
            input_paused: false,
            owned: None,
            raw_owned: false,
        }
    }

    /// Install the sole fence-capable reader before entering terminal modes.
    /// # Errors
    /// Rejects replacement after any terminal/input owner has been initialized.
    pub fn install_fenced_input(
        &mut self,
        input: impl FencedTerminalInput + 'static,
    ) -> io::Result<()> {
        if self.raw_owned
            || self.events.is_some()
            || self.fenced_input.is_some()
            || self.owned.is_some()
        {
            return Err(io::Error::other("terminal input already initialized"));
        }
        self.fenced_input = Some(Box::new(input));
        Ok(())
    }

    /// Pause and drain the actual reader before acknowledging an application barrier.
    /// # Errors
    /// Legacy/unknown backends fail closed. Failure preserves paused admission.
    pub fn fence_input(&mut self) -> io::Result<u64> {
        self.input_paused = true;
        self.input_generation = self
            .input_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("terminal input generation exhausted"))?;
        self.fenced_input
            .as_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "terminal reader has no acknowledged drain barrier",
                )
            })?
            .drain_and_reset()?;
        Ok(self.input_generation)
    }

    /// Resume only the exact generation whose application queue was discarded.
    /// # Errors
    /// Rejects stale generations or unsupported/unacknowledged backends.
    pub fn resume_input(&mut self, generation: u64) -> io::Result<()> {
        if !self.input_paused || generation != self.input_generation {
            return Err(io::Error::other("stale terminal input barrier"));
        }
        let input = self.fenced_input.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "terminal reader has no acknowledged drain barrier",
            )
        })?;
        input.drain_and_reset()?;
        input.resume()?;
        self.input_paused = false;
        Ok(())
    }

    /// Enter raw mode and acquire the requested outer-terminal modes.
    ///
    /// # Errors
    /// Reports entry failures and any failure to restore partially acquired modes.
    #[allow(unused_mut)] // Only non-Linux backends probe protocol negotiation.
    pub fn enter(&mut self, mut options: TerminalOptions) -> io::Result<()> {
        if self.raw_owned || self.owned.is_some() {
            return Err(io::Error::other("terminal modes already owned"));
        }
        enable_raw_mode()?;
        self.raw_owned = true;
        #[cfg(not(target_os = "linux"))]
        if options.keyboard_enhanced {
            options.keyboard_enhanced =
                crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
        }
        // Mark requested modes before writing: a partial write can enable any
        // prefix, and every requested mode must then be undone during repair.
        self.owned = Some(options);
        if let Err(entry) =
            queue_entry(&mut self.writer, options).and_then(|()| self.writer.flush())
        {
            return match self.restore() {
                Ok(()) => Err(entry),
                Err(restore) => Err(io::Error::other(format!(
                    "terminal entry failed: {entry}; restoration failed: {restore}"
                ))),
            };
        }
        #[cfg(target_os = "linux")]
        if self.fenced_input.is_none() {
            self.fenced_input = Some(Box::new(RawFencedInput {
                reader: bmux_tui::crossterm::input::RawInput::open_stdout_tty()?,
                paused: false,
                wake: Box::pin(tokio::time::sleep(std::time::Duration::ZERO)),
            }));
        }
        #[cfg(not(target_os = "linux"))]
        if self.fenced_input.is_none() {
            self.events = Some(EventStream::new());
        }
        Ok(())
    }

    /// Whether the explicitly requested keyboard protocol was negotiated.
    #[must_use]
    pub fn keyboard_enhanced(&self) -> bool {
        self.owned.is_some_and(|options| options.keyboard_enhanced)
    }

    /// Await one unmodified outer-terminal event, without a repaint timer.
    ///
    /// # Errors
    /// Reports an uninitialized or failed terminal event stream.
    pub async fn next_event(&mut self) -> io::Result<Option<Event>> {
        if self.input_paused {
            return Err(io::Error::other("terminal input is fenced"));
        }
        if let Some(input) = self.fenced_input.as_mut() {
            return futures_util::future::poll_fn(|cx| input.poll_event(cx)).await;
        }
        let events = self
            .events
            .as_mut()
            .ok_or_else(|| io::Error::other("terminal input not initialized"))?;
        events.next().await.transpose()
    }

    /// Current physical terminal geometry; callers own resize acknowledgment.
    ///
    /// # Errors
    /// Reports terminal geometry query errors.
    #[allow(clippy::unused_self)] // Geometry belongs to the terminal backend interface, not consumer policy.
    pub fn geometry(&self) -> io::Result<(u16, u16)> {
        crossterm::terminal::size()
    }

    /// Restore all owned modes, attempting raw-mode cleanup even if output fails.
    /// Failed ownership remains tracked so drop can retry; success is explicit.
    ///
    /// # Errors
    /// Reports output/flush and raw-mode restoration failures.
    pub fn restore(&mut self) -> io::Result<()> {
        self.events = None;
        self.fenced_input = None;
        self.input_paused = true;
        let display = self.owned.map_or(Ok(()), |options| {
            queue_restore(&mut self.writer, options).and_then(|()| self.writer.flush())
        });
        if display.is_ok() {
            self.owned = None;
        }
        let raw = if self.raw_owned {
            disable_raw_mode()
        } else {
            Ok(())
        };
        if raw.is_ok() {
            self.raw_owned = false;
        }
        match (display, raw) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(display), Err(raw)) => Err(io::Error::other(format!(
                "terminal output restoration failed: {display}; raw-mode restoration failed: {raw}"
            ))),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        }
    }
}

impl<W: Write> Write for TerminalIo<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Drop for TerminalIo<W> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn queue_entry<W: Write>(writer: &mut W, options: TerminalOptions) -> io::Result<()> {
    if options.alternate_screen {
        queue!(writer, EnterAlternateScreen)?;
    }
    if options.keyboard_enhanced {
        queue!(
            writer,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
    }
    if options.mouse_capture {
        queue!(writer, EnableMouseCapture)?;
    }
    if options.bracketed_paste {
        queue!(writer, EnableBracketedPaste)?;
    }
    if options.focus_reporting {
        queue!(writer, EnableFocusChange)?;
    }
    if options.disable_wrap {
        writer.write_all(b"\x1b[?7l")?;
    }
    Ok(())
}

fn queue_restore<W: Write>(writer: &mut W, options: TerminalOptions) -> io::Result<()> {
    if options.mouse_capture {
        queue!(writer, DisableMouseCapture)?;
    }
    if options.bracketed_paste {
        queue!(writer, DisableBracketedPaste)?;
    }
    if options.focus_reporting {
        queue!(writer, DisableFocusChange)?;
    }
    if options.keyboard_enhanced {
        queue!(writer, PopKeyboardEnhancementFlags)?;
    }
    if options.disable_wrap {
        writer.write_all(b"\x1b[?7h")?;
    }
    if options.alternate_screen {
        writer.write_all(b"\x1b[?2026l\x1b[0m\x1b[?25h")?;
        queue!(writer, LeaveAlternateScreen)?;
    }
    Ok(())
}

/// Deliver an already buffered frame. Commit cursor, damage and render baselines
/// only after this returns success; invalidate them on any error.
///
/// # Errors
/// Reports either a partial/failed write or a failed flush.
pub fn write_frame<W: Write + ?Sized>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_modes_restore_only_acquired_controls() {
        let options = TerminalOptions {
            bracketed_paste: true,
            ..TerminalOptions::default()
        };
        let mut bytes = Vec::new();
        queue_entry(&mut bytes, options).unwrap();
        queue_restore(&mut bytes, options).unwrap();
        assert_eq!(bytes, b"\x1b[?2004h\x1b[?2004l");
        bytes.clear();
        queue_restore(&mut bytes, TerminalOptions::default()).unwrap();
        assert!(bytes.is_empty());
    }

    #[test]
    fn undecorated_modes_restore_wrap_focus_and_screen_without_keyboard_push() {
        let options = TerminalOptions {
            alternate_screen: true,
            disable_wrap: true,
            focus_reporting: true,
            mouse_capture: true,
            bracketed_paste: true,
            keyboard_enhanced: false,
        };
        let mut bytes = Vec::new();
        queue_entry(&mut bytes, options).unwrap();
        queue_restore(&mut bytes, options).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        for control in [
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[?7l",
            "\x1b[?7h",
            "\x1b[?1004l",
            "\x1b[?25h",
        ] {
            assert!(text.contains(control));
        }
        assert!(!text.contains("\x1b[>"));
    }

    #[test]
    fn failed_restoration_retains_ownership_and_is_not_success() {
        struct FailWrite;
        impl Write for FailWrite {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("terminal disconnected"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        // Inject only display ownership; this test never changes process tty modes.
        let mut terminal = TerminalIo::new(FailWrite);
        terminal.owned = Some(TerminalOptions {
            bracketed_paste: true,
            ..TerminalOptions::default()
        });
        assert!(terminal.restore().is_err());
        assert!(terminal.owned.is_some());
        assert!(!terminal.raw_owned);
    }

    #[test]
    fn frame_delivery_reports_partial_write_failure() {
        struct PartialWrite(bool);
        impl Write for PartialWrite {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("partial frame"));
                }
                self.0 = true;
                Ok(bytes.len().min(1))
            }
            fn flush(&mut self) -> io::Result<()> {
                panic!("failed write must not flush")
            }
        }
        assert!(write_frame(&mut PartialWrite(false), b"frame").is_err());
    }

    #[test]
    fn frame_delivery_reports_flush_failure() {
        struct FailFlush(Vec<u8>);
        impl Write for FailFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush failed"))
            }
        }
        let mut writer = FailFlush(Vec::new());
        assert!(write_frame(&mut writer, b"frame").is_err());
        assert_eq!(writer.0, b"frame");
    }
}
