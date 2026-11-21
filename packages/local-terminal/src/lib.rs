//! The server side of bmux's plugin-less local terminal, over any stream.
//!
//! The protocol: the client sends [`HELLO`] and its terminal size (columns, then rows, each
//! two bytes big-endian); the server answers [`HELLO`]. Then the client sends frames, each a
//! tag byte: `0` detaches, `1` is input (a two-byte length and the bytes), `2` is a resize (four
//! size bytes). The server sends output as a four-byte length and the bytes (at most
//! [`LIMIT`]), and when the shell exits, a zero length and its exit code.
//!
//! The shell outlives an attachment: [`serve_attachment`] keeps it in the caller's slot, so
//! the next attachment continues it, with the output it wrote meanwhile.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery)]

use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use bmux_pty::{PtyCommand, PtyEvent, PtyProcess, PtySize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// What each side says first.
pub const HELLO: &[u8; 8] = b"BMUXPTY1";
/// The most output bytes in one frame.
pub const LIMIT: usize = 64 * 1024;
/// How long a client has to say hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the server forwards the shell's output.
pub const OUTPUT_INTERVAL: Duration = Duration::from_millis(10);

/// A terminal size from its four wire bytes (columns, then rows).
#[must_use]
pub const fn size_from_bytes(bytes: [u8; 4]) -> PtySize {
    PtySize::new(
        u16::from_be_bytes([bytes[0], bytes[1]]),
        u16::from_be_bytes([bytes[2], bytes[3]]),
    )
}

/// A terminal size's four wire bytes.
#[must_use]
pub const fn size_to_bytes(size: PtySize) -> [u8; 4] {
    let cols = size.cols.to_be_bytes();
    let rows = size.rows.to_be_bytes();
    [cols[0], cols[1], rows[0], rows[1]]
}

/// Serve one attachment on `stream`, running `shell` in `slot` (started if empty), until the
/// client detaches, the stream ends, or the shell exits (which empties the slot).
///
/// # Errors
///
/// * If the client does not say hello in time or is incompatible
/// * If the shell cannot be started, resized or written to
/// * If the stream fails, or a frame is invalid
pub async fn serve_attachment<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    shell: &str,
    slot: &mut Option<PtyProcess>,
) -> Result<()> {
    let mut hello = [0; 12];
    switchy_async::time::timeout(HELLO_TIMEOUT, stream.read_exact(&mut hello)).await??;
    ensure!(&hello[..8] == HELLO, "incompatible single terminal client");
    let initial = size_from_bytes([hello[8], hello[9], hello[10], hello[11]]);
    if slot.is_none() {
        *slot = Some(PtyCommand::new(shell).spawn(initial)?);
    }
    let process = slot.as_mut().context("missing terminal execution")?;
    process.resize(initial)?;
    let writer = process.writer()?;
    stream.write_all(HELLO).await?;
    let (mut read, mut write) = tokio::io::split(stream);
    let control = process.control();
    let input = async {
        loop {
            match read.read_u8().await? {
                0 => return Ok::<(), anyhow::Error>(()),
                1 => {
                    let len = usize::from(read.read_u16().await?);
                    let mut buf = vec![0; len];
                    read.read_exact(&mut buf).await?;
                    writer.write(buf)?;
                }
                2 => {
                    let mut buf = [0; 4];
                    read.read_exact(&mut buf).await?;
                    control.resize(size_from_bytes(buf))?;
                }
                _ => anyhow::bail!("invalid terminal control"),
            }
        }
    };
    let output = async {
        let mut tick = switchy_async::time::interval(OUTPUT_INTERVAL);
        loop {
            tick.tick().await;
            for event in process.ready_events(16) {
                match event {
                    PtyEvent::Output(bytes) | PtyEvent::Stdout(bytes) => {
                        for chunk in bytes.chunks(LIMIT) {
                            write.write_u32(u32::try_from(chunk.len())?).await?;
                            write.write_all(chunk).await?;
                        }
                    }
                    PtyEvent::Exited(status) => {
                        write.write_u32(0).await?;
                        write
                            .write_u8(
                                status
                                    .code()
                                    .and_then(|c| u8::try_from(c).ok())
                                    .unwrap_or(1),
                            )
                            .await?;
                        return Ok::<(), anyhow::Error>(());
                    }
                }
            }
        }
    };
    let result = switchy_async::select! {
        result = input => result,
        result = output => result,
    };
    if process.has_exited() {
        *slot = None;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_trip_through_their_wire_bytes() {
        let size = PtySize::new(41, 133);
        assert_eq!(size_from_bytes(size_to_bytes(size)), size);
    }
}
