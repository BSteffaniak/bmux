//! Stream-level compression wrapper for remote IPC transports.
//!
//! Wraps any `AsyncRead + AsyncWrite` stream with per-flush block compression.
//! Each `flush()` compresses the buffered writes into a single block prefixed
//! with its compressed and uncompressed lengths.  Reads decompress blocks on
//! demand.
//!
//! Block format on the wire:
//!
//! ```text
//! [4-byte LE compressed_len][4-byte LE uncompressed_len][compressed_bytes]
//! ```
//!
//! This is intentionally simpler than zstd's streaming API to avoid complex
//! async state machine management while still achieving good compression ratios
//! (each block is a complete zstd frame with its own dictionary).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A compression/decompression wrapper around an async duplex stream.
///
/// Writes are buffered until `poll_flush` is called, at which point the
/// buffered data is compressed and written as a single block.  Reads
/// transparently decompress incoming blocks.
pub struct CompressedStream<S> {
    inner: S,
    /// Compression level (zstd).
    #[cfg_attr(not(feature = "compression-zstd"), allow(dead_code))]
    level: i32,
    // ── Write state ──────────────────────────────────────────────────────
    write_buf: Vec<u8>,
    /// Compressed bytes waiting to be written to `inner`.
    write_out: Vec<u8>,
    /// How many bytes of `write_out` have been flushed.
    write_out_pos: usize,
    // ── Read state ───────────────────────────────────────────────────────
    /// Decompressed bytes from the current inbound block.
    read_buf: Vec<u8>,
    /// Current read position within `read_buf`.
    read_pos: usize,
    /// Partial header bytes being accumulated.
    header_buf: Vec<u8>,
    /// State of the read FSM.
    read_state: ReadState,
    /// Compressed block bytes being accumulated.
    block_buf: Vec<u8>,
    /// Expected compressed block length (from header).
    block_compressed_len: usize,
    /// Expected uncompressed block length (from header, for validation).
    block_uncompressed_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    /// Waiting for / reading the 8-byte block header.
    Header,
    /// Reading compressed block bytes.
    Block,
    /// Decompressed data available in read_buf.
    Data,
}

const BLOCK_HEADER_LEN: usize = 8; // 4 bytes compressed_len + 4 bytes uncompressed_len

impl<S> CompressedStream<S> {
    /// Wrap `inner` with block-based zstd compression at the given level.
    pub fn new(inner: S, level: i32) -> Self {
        Self {
            inner,
            level,
            write_buf: Vec::with_capacity(64 * 1024),
            write_out: Vec::new(),
            write_out_pos: 0,
            read_buf: Vec::new(),
            read_pos: 0,
            header_buf: Vec::with_capacity(BLOCK_HEADER_LEN),
            read_state: ReadState::Header,
            block_buf: Vec::new(),
            block_compressed_len: 0,
            block_uncompressed_len: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CompressedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            // If we have decompressed data, serve it.
            if this.read_state == ReadState::Data && this.read_pos < this.read_buf.len() {
                let available = &this.read_buf[this.read_pos..];
                let to_copy = available.len().min(buf.remaining());
                buf.put_slice(&available[..to_copy]);
                this.read_pos += to_copy;
                if this.read_pos >= this.read_buf.len() {
                    // Block fully consumed, prepare for next header.
                    this.read_buf.clear();
                    this.read_pos = 0;
                    this.read_state = ReadState::Header;
                    this.header_buf.clear();
                }
                return Poll::Ready(Ok(()));
            }

            match this.read_state {
                ReadState::Header => {
                    // Read header bytes incrementally.
                    while this.header_buf.len() < BLOCK_HEADER_LEN {
                        let mut tmp = [0u8; 1];
                        let mut tmp_buf = ReadBuf::new(&mut tmp);
                        match Pin::new(&mut this.inner).poll_read(cx, &mut tmp_buf) {
                            Poll::Ready(Ok(())) => {
                                if tmp_buf.filled().is_empty() {
                                    // EOF
                                    return Poll::Ready(Ok(()));
                                }
                                this.header_buf.push(tmp[0]);
                            }
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    // Parse header.
                    let compressed_len = u32::from_le_bytes([
                        this.header_buf[0],
                        this.header_buf[1],
                        this.header_buf[2],
                        this.header_buf[3],
                    ]) as usize;
                    let uncompressed_len = u32::from_le_bytes([
                        this.header_buf[4],
                        this.header_buf[5],
                        this.header_buf[6],
                        this.header_buf[7],
                    ]) as usize;
                    this.block_compressed_len = compressed_len;
                    this.block_uncompressed_len = uncompressed_len;
                    this.block_buf.clear();
                    this.block_buf.reserve(compressed_len);
                    this.read_state = ReadState::Block;
                }
                ReadState::Block => {
                    // Read compressed block bytes incrementally.
                    while this.block_buf.len() < this.block_compressed_len {
                        let remaining = this.block_compressed_len - this.block_buf.len();
                        let mut tmp = vec![0u8; remaining.min(8192)];
                        let mut tmp_buf = ReadBuf::new(&mut tmp);
                        match Pin::new(&mut this.inner).poll_read(cx, &mut tmp_buf) {
                            Poll::Ready(Ok(())) => {
                                let n = tmp_buf.filled().len();
                                if n == 0 {
                                    return Poll::Ready(Err(io::Error::new(
                                        io::ErrorKind::UnexpectedEof,
                                        "compressed block truncated",
                                    )));
                                }
                                this.block_buf.extend_from_slice(&tmp[..n]);
                            }
                            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    // Decompress the block.
                    #[cfg(feature = "compression-zstd")]
                    {
                        match zstd::bulk::decompress(&this.block_buf, this.block_uncompressed_len) {
                            Ok(data) => {
                                this.read_buf = data;
                                this.read_pos = 0;
                                this.read_state = ReadState::Data;
                            }
                            Err(e) => {
                                return Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!("zstd decompress failed: {e}"),
                                )));
                            }
                        }
                    }
                    #[cfg(not(feature = "compression-zstd"))]
                    {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "transport compression requires zstd feature",
                        )));
                    }
                }
                ReadState::Data => {
                    // Handled at the top of the loop.
                    unreachable!();
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CompressedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Buffer writes; actual compression happens on flush.
        let this = self.get_mut();
        this.write_buf.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // If there's buffered write data, compress it into write_out.
        if !this.write_buf.is_empty() && this.write_out_pos >= this.write_out.len() {
            #[cfg(feature = "compression-zstd")]
            {
                let compressed =
                    zstd::bulk::compress(&this.write_buf, this.level).map_err(io::Error::other)?;
                #[allow(clippy::cast_possible_truncation)]
                let compressed_len = compressed.len() as u32;
                #[allow(clippy::cast_possible_truncation)]
                let uncompressed_len = this.write_buf.len() as u32;
                this.write_out.clear();
                this.write_out.reserve(BLOCK_HEADER_LEN + compressed.len());
                this.write_out
                    .extend_from_slice(&compressed_len.to_le_bytes());
                this.write_out
                    .extend_from_slice(&uncompressed_len.to_le_bytes());
                this.write_out.extend_from_slice(&compressed);
                this.write_out_pos = 0;
                this.write_buf.clear();
            }
            #[cfg(not(feature = "compression-zstd"))]
            {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "transport compression requires zstd feature",
                )));
            }
        }

        // Flush write_out to inner stream.
        while this.write_out_pos < this.write_out.len() {
            let remaining = &this.write_out[this.write_out_pos..];
            match Pin::new(&mut this.inner).poll_write(cx, remaining) {
                Poll::Ready(Ok(n)) => {
                    if n == 0 {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "zero-length write on compressed stream",
                        )));
                    }
                    this.write_out_pos += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // Flush the inner stream.
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        // Flush any remaining buffered data before shutting down.
        if !this.write_buf.is_empty() {
            match Pin::new(&mut *this).poll_flush(cx) {
                Poll::Ready(Ok(())) => {}
                other => return other,
            }
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

// Re-derive Unpin since all inner fields are Unpin-compatible.
impl<S: Unpin> Unpin for CompressedStream<S> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "compression-zstd")]
    #[tokio::test]
    async fn compressed_stream_roundtrip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Create a duplex pair to simulate a network connection.
        let (client, server) = tokio::io::duplex(64 * 1024);

        let mut writer = CompressedStream::new(client, 1);
        let mut reader = CompressedStream::new(server, 1);

        let data = b"Hello, compressed world! This is a test payload that should compress.";
        let repeated: Vec<u8> = data.iter().copied().cycle().take(4096).collect();

        // Write + flush in a spawned task.
        let write_data = repeated.clone();
        let write_task = tokio::spawn(async move {
            writer.write_all(&write_data).await.unwrap();
            writer.flush().await.unwrap();
            writer.shutdown().await.unwrap();
        });

        // Read everything.
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await.unwrap();

        write_task.await.unwrap();
        assert_eq!(received, repeated);
    }
}
