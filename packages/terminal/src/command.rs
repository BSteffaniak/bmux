//! Output commands and the [`queue!`](crate::queue) / [`execute!`](crate::execute) macros.

use std::io::{self, Write};

/// A terminal output command.
///
/// [`Command::write_ansi`] writes the command's escape sequence as bytes. On
/// Windows a command may instead be carried out through the console API when
/// [`Command::is_ansi_code_supported`] reports that the console cannot process
/// the sequence.
pub trait Command {
    /// Write this command's ANSI escape sequence.
    ///
    /// # Errors
    ///
    /// Returns any error reported by the destination writer.
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()>;

    /// Carry the command out through the Windows console API.
    ///
    /// # Errors
    ///
    /// Returns the console API failure, or `Unsupported` when the command has
    /// no console API implementation.
    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal command requires virtual terminal processing",
        ))
    }

    /// Whether this command should be written as an ANSI sequence.
    ///
    /// The default enables virtual terminal output processing on first use and
    /// reports whether it is available.
    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        crate::sys::windows::supports_ansi()
    }
}

impl<T: Command + ?Sized> Command for &T {
    #[inline]
    fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
        (**self).write_ansi(out)
    }

    #[cfg(windows)]
    #[inline]
    fn execute_winapi(&self) -> io::Result<()> {
        (**self).execute_winapi()
    }

    #[cfg(windows)]
    #[inline]
    fn is_ansi_code_supported(&self) -> bool {
        (**self).is_ansi_code_supported()
    }
}

/// Queue a command on a writer without flushing.
pub trait QueueableCommand {
    /// Write `command` into this writer's buffer.
    ///
    /// # Errors
    ///
    /// Returns any write or console API error.
    fn queue(&mut self, command: impl Command) -> io::Result<&mut Self>;
}

/// Queue a command on a writer and flush it.
pub trait ExecutableCommand {
    /// Write `command` and flush the writer.
    ///
    /// # Errors
    ///
    /// Returns any write, flush, or console API error.
    fn execute(&mut self, command: impl Command) -> io::Result<&mut Self>;
}

impl<T: Write + ?Sized> QueueableCommand for T {
    #[inline]
    fn queue(&mut self, command: impl Command) -> io::Result<&mut Self> {
        #[cfg(windows)]
        if !command.is_ansi_code_supported() {
            // Earlier queued output must reach the console before the API call
            // changes its state.
            self.flush()?;
            command.execute_winapi()?;
            return Ok(self);
        }
        command.write_ansi(self)?;
        Ok(self)
    }
}

impl<T: Write + ?Sized> ExecutableCommand for T {
    #[inline]
    fn execute(&mut self, command: impl Command) -> io::Result<&mut Self> {
        self.queue(command)?;
        self.flush()?;
        Ok(self)
    }
}

/// Queue one or more commands on a writer without flushing.
///
/// Accepts both an owned writer expression and a `&mut` writer. Commands are
/// written in order; the first failure stops the sequence and is returned.
#[macro_export]
macro_rules! queue {
    ($writer:expr $(, $command:expr)* $(,)?) => {{
        use ::std::io::Write as _;
        ::std::result::Result::<_, ::std::io::Error>::Ok($writer.by_ref())
            $(.and_then(|writer| $crate::QueueableCommand::queue(writer, $command)))*
            .map(|_| ())
    }};
}

/// Queue one or more commands on a writer, then flush it.
#[macro_export]
macro_rules! execute {
    ($writer:expr $(, $command:expr)* $(,)?) => {{
        use ::std::io::Write as _;
        $crate::queue!($writer $(, $command)*)
            .and_then(|()| ::std::io::Write::flush($writer.by_ref()))
    }};
}

/// Write a decimal number without going through `fmt`.
#[inline]
pub(crate) fn write_u16(out: &mut (impl Write + ?Sized), value: u16) -> io::Result<()> {
    let mut digits = [0u8; 5];
    let mut index = digits.len();
    let mut value = value;
    loop {
        index -= 1;
        // `value % 10` is below 10 and always fits in a byte.
        #[allow(clippy::cast_possible_truncation)]
        let digit = (value % 10) as u8;
        digits[index] = b'0' + digit;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.write_all(&digits[index..])
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Marker;
    impl Command for Marker {
        fn write_ansi(&self, out: &mut (impl Write + ?Sized)) -> io::Result<()> {
            out.write_all(b"<m>")
        }
        #[cfg(windows)]
        fn is_ansi_code_supported(&self) -> bool {
            true
        }
    }

    #[test]
    fn queue_writes_commands_in_order_without_flushing() {
        let mut out = Vec::new();
        queue!(out, Marker, &Marker).unwrap();
        assert_eq!(out, b"<m><m>");
    }

    #[test]
    fn execute_accepts_mutable_references() {
        let mut out = Vec::new();
        let writer = &mut out;
        execute!(writer, Marker).unwrap();
        assert_eq!(out, b"<m>");
    }

    #[test]
    fn decimal_writers_match_display() {
        for value in [0u16, 1, 9, 10, 99, 100, 12345, u16::MAX] {
            let mut out = Vec::new();
            write_u16(&mut out, value).unwrap();
            assert_eq!(out, value.to_string().as_bytes());
        }
    }
}
