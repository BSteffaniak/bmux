use crate::frame::{
    FrameDecodeError, FrameEncodeError, decode_frame_compressed, decode_frame_exact, encode_frame,
    encode_frame_compressed,
};
use crate::{Envelope, IpcEndpoint};
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

#[cfg(windows)]
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};

/// Local IPC listener abstraction.
#[derive(Debug)]
pub struct LocalIpcListener {
    inner: ListenerInner,
}

#[derive(Debug)]
enum ListenerInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    WindowsNamedPipe { pipe_name: String },
}

/// Local IPC stream abstraction.
#[derive(Debug)]
pub struct LocalIpcStream {
    inner: StreamInner,
}

/// Trait alias for erased async duplex streams used by framed IPC wrappers.
pub trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Framed IPC stream backed by an erased async duplex transport.
pub struct ErasedIpcStream {
    inner: Box<dyn AsyncReadWrite>,
    /// Optional frame compression codec (Layer 2).
    frame_codec: Option<Arc<dyn crate::compression::CompressionCodec>>,
    /// Whether to expect the compressed frame format on reads.
    compressed_frames: bool,
}

impl std::fmt::Debug for ErasedIpcStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ErasedIpcStream(..)")
    }
}

#[derive(Debug)]
enum StreamInner {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    WindowsServer(NamedPipeServer),
    #[cfg(windows)]
    WindowsClient(NamedPipeClient),
}

/// Errors returned by local IPC transport operations.
#[derive(Debug, Error)]
pub enum IpcTransportError {
    #[error("unsupported endpoint for this platform")]
    UnsupportedEndpoint,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame encoding failed: {0}")]
    FrameEncode(#[from] FrameEncodeError),
    #[error("frame decoding failed: {0}")]
    FrameDecode(#[from] FrameDecodeError),
}

impl LocalIpcListener {
    /// Bind a local listener for the provided endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint is unsupported on this platform or
    /// the listener cannot be created.
    pub async fn bind(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        #[cfg(unix)]
        {
            if let IpcEndpoint::UnixSocket(path) = endpoint {
                prepare_unix_socket_path(path)?;
                let listener = tokio::net::UnixListener::bind(path)?;
                return Ok(Self {
                    inner: ListenerInner::Unix(listener),
                });
            }
        }

        #[cfg(windows)]
        {
            if let IpcEndpoint::WindowsNamedPipe(pipe_name) = endpoint {
                return Ok(Self {
                    inner: ListenerInner::WindowsNamedPipe {
                        pipe_name: pipe_name.clone(),
                    },
                });
            }
        }

        Err(IpcTransportError::UnsupportedEndpoint)
    }

    /// Accept an incoming local connection.
    ///
    /// # Errors
    ///
    /// Returns an error when accepting fails.
    pub async fn accept(&self) -> Result<LocalIpcStream, IpcTransportError> {
        match &self.inner {
            #[cfg(unix)]
            ListenerInner::Unix(listener) => {
                let (stream, _) = listener.accept().await?;
                Ok(LocalIpcStream {
                    inner: StreamInner::Unix(stream),
                })
            }
            #[cfg(windows)]
            ListenerInner::WindowsNamedPipe { pipe_name } => {
                let server = ServerOptions::new().create(pipe_name)?;
                server.connect().await?;
                Ok(LocalIpcStream {
                    inner: StreamInner::WindowsServer(server),
                })
            }
        }
    }
}

/// Write half of a split [`LocalIpcStream`].
pub struct IpcStreamWriter {
    inner: tokio::io::WriteHalf<LocalIpcStream>,
    /// Optional frame compression codec, activated after capability negotiation.
    frame_codec: Option<Arc<dyn crate::compression::CompressionCodec>>,
}

impl std::fmt::Debug for IpcStreamWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IpcStreamWriter(..)")
    }
}

/// Read half of a split [`LocalIpcStream`].
pub struct IpcStreamReader {
    inner: tokio::io::ReadHalf<LocalIpcStream>,
    /// Whether to expect the compressed frame format (1-byte compression id
    /// prefix).  Activated after capability negotiation.
    compressed_frames: bool,
}

impl std::fmt::Debug for IpcStreamReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IpcStreamReader(..)")
    }
}

impl IpcStreamWriter {
    /// Send a single framed envelope.
    ///
    /// Uses compressed frame format if a frame codec has been set via
    /// [`enable_frame_compression`](Self::enable_frame_compression).
    ///
    /// # Errors
    ///
    /// Returns an error if frame encoding or socket writes fail.
    pub async fn send_envelope(&mut self, envelope: &Envelope) -> Result<(), IpcTransportError> {
        let frame = if self.frame_codec.is_some() {
            encode_frame_compressed(envelope, self.frame_codec.as_deref())?
        } else {
            encode_frame(envelope)?
        };
        write_frame(&mut self.inner, &frame).await
    }

    /// Write a pre-encoded frame (length-prefixed bytes) directly to the socket.
    ///
    /// # Errors
    ///
    /// Returns an error if writing fails.
    pub async fn write_raw_frame(&mut self, frame: &[u8]) -> Result<(), IpcTransportError> {
        write_frame(&mut self.inner, frame).await
    }

    /// Enable frame-level compression for all subsequent `send_envelope` calls.
    pub fn enable_frame_compression(
        &mut self,
        codec: Arc<dyn crate::compression::CompressionCodec>,
    ) {
        self.frame_codec = Some(codec);
    }
}

impl IpcStreamReader {
    /// Receive a single framed envelope.
    ///
    /// Uses compressed frame format if compression was enabled via
    /// [`enable_frame_compression`](Self::enable_frame_compression).
    ///
    /// # Errors
    ///
    /// Returns an error if frame reads fail or the frame is invalid.
    pub async fn recv_envelope(&mut self) -> Result<Envelope, IpcTransportError> {
        if self.compressed_frames {
            read_frame_compressed(&mut self.inner).await
        } else {
            read_frame(&mut self.inner).await
        }
    }

    /// Enable compressed frame format for all subsequent `recv_envelope` calls.
    pub const fn enable_frame_compression(&mut self) {
        self.compressed_frames = true;
    }
}

impl LocalIpcStream {
    /// Split this stream into independent read and write halves.
    ///
    /// This allows concurrent reading and writing on the same underlying
    /// connection, which is required for server-push event delivery alongside
    /// request/response traffic.
    #[must_use]
    pub fn into_split(self) -> (IpcStreamReader, IpcStreamWriter) {
        let (read_half, write_half) = tokio::io::split(self);
        (
            IpcStreamReader {
                inner: read_half,
                compressed_frames: false,
            },
            IpcStreamWriter {
                inner: write_half,
                frame_codec: None,
            },
        )
    }

    /// Connect to a local endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint is unsupported on this platform or
    /// the connection fails.
    pub async fn connect(endpoint: &IpcEndpoint) -> Result<Self, IpcTransportError> {
        #[cfg(unix)]
        {
            if let IpcEndpoint::UnixSocket(path) = endpoint {
                let stream = tokio::net::UnixStream::connect(path).await?;
                return Ok(Self {
                    inner: StreamInner::Unix(stream),
                });
            }
        }

        #[cfg(windows)]
        {
            if let IpcEndpoint::WindowsNamedPipe(pipe_name) = endpoint {
                let stream = ClientOptions::new().open(pipe_name)?;
                return Ok(Self {
                    inner: StreamInner::WindowsClient(stream),
                });
            }
        }

        Err(IpcTransportError::UnsupportedEndpoint)
    }

    /// Send a single framed envelope.
    ///
    /// # Errors
    ///
    /// Returns an error if frame encoding or socket writes fail.
    pub async fn send_envelope(&mut self, envelope: &Envelope) -> Result<(), IpcTransportError> {
        let frame = encode_frame(envelope)?;

        match &mut self.inner {
            #[cfg(unix)]
            StreamInner::Unix(stream) => write_frame(stream, &frame).await,
            #[cfg(windows)]
            StreamInner::WindowsServer(stream) => write_frame(stream, &frame).await,
            #[cfg(windows)]
            StreamInner::WindowsClient(stream) => write_frame(stream, &frame).await,
        }
    }

    /// Receive a single framed envelope.
    ///
    /// # Errors
    ///
    /// Returns an error if frame reads fail or the frame is invalid.
    pub async fn recv_envelope(&mut self) -> Result<Envelope, IpcTransportError> {
        match &mut self.inner {
            #[cfg(unix)]
            StreamInner::Unix(stream) => read_frame(stream).await,
            #[cfg(windows)]
            StreamInner::WindowsServer(stream) => read_frame(stream).await,
            #[cfg(windows)]
            StreamInner::WindowsClient(stream) => read_frame(stream).await,
        }
    }
}

impl ErasedIpcStream {
    #[must_use]
    pub fn new(inner: Box<dyn AsyncReadWrite>) -> Self {
        Self {
            inner,
            frame_codec: None,
            compressed_frames: false,
        }
    }

    pub async fn send_envelope(&mut self, envelope: &Envelope) -> Result<(), IpcTransportError> {
        let frame = if self.frame_codec.is_some() {
            encode_frame_compressed(envelope, self.frame_codec.as_deref())?
        } else {
            encode_frame(envelope)?
        };
        write_frame(&mut self.inner, &frame).await
    }

    pub async fn recv_envelope(&mut self) -> Result<Envelope, IpcTransportError> {
        if self.compressed_frames {
            read_frame_compressed(&mut self.inner).await
        } else {
            read_frame(&mut self.inner).await
        }
    }

    /// Enable frame-level compression for all subsequent sends.
    pub fn enable_frame_compression(
        &mut self,
        codec: Arc<dyn crate::compression::CompressionCodec>,
    ) {
        self.frame_codec = Some(codec);
    }

    /// Enable compressed frame format for all subsequent receives.
    pub const fn enable_frame_decompression(&mut self) {
        self.compressed_frames = true;
    }
}

// ── AsyncRead + AsyncWrite delegation for raw I/O ────────────────────────────
//
// These impls allow `LocalIpcStream` to be used with `BufReader`, `BufWriter`,
// `tokio::io::split()`, etc. — enabling line-based protocols (like the playbook
// interactive NDJSON protocol) on top of the cross-platform transport.
//
// Safety: all inner stream types (`UnixStream`, `NamedPipeServer`,
// `NamedPipeClient`) implement `Unpin`, so `Pin::new(s)` is sound.

impl AsyncRead for LocalIpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for LocalIpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.get_mut().inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            #[cfg(unix)]
            StreamInner::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            StreamInner::WindowsServer(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            StreamInner::WindowsClient(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

async fn write_frame<T>(stream: &mut T, frame: &[u8]) -> Result<(), IpcTransportError>
where
    T: AsyncWrite + Unpin,
{
    stream.write_all(frame).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame<T>(stream: &mut T) -> Result<Envelope, IpcTransportError>
where
    T: AsyncRead + Unpin,
{
    let mut len_bytes = [0_u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let payload_len = u32::from_le_bytes(len_bytes) as usize;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&len_bytes);
    frame.resize(4 + payload_len, 0);
    stream.read_exact(&mut frame[4..]).await?;
    let envelope = decode_frame_exact(&frame)?;
    Ok(envelope)
}

/// Read one frame using the compressed frame format (1-byte compression id
/// prefix after the length).
async fn read_frame_compressed<T>(stream: &mut T) -> Result<Envelope, IpcTransportError>
where
    T: AsyncRead + Unpin,
{
    let mut len_bytes = [0_u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let total_len = u32::from_le_bytes(len_bytes) as usize;
    let mut frame = Vec::with_capacity(4 + total_len);
    frame.extend_from_slice(&len_bytes);
    frame.resize(4 + total_len, 0);
    stream.read_exact(&mut frame[4..]).await?;
    let envelope = decode_frame_compressed(&frame)?;
    Ok(envelope)
}

#[cfg(unix)]
fn prepare_unix_socket_path(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{IpcTransportError, LocalIpcListener, LocalIpcStream};
    use crate::{Envelope, EnvelopeKind, IpcEndpoint, Request, decode, encode};
    use uuid::Uuid;

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_transport_roundtrip_between_client_and_server() {
        let socket_path = std::env::temp_dir().join(format!("bmux-ipc-{}.sock", Uuid::new_v4()));
        let endpoint = IpcEndpoint::unix_socket(&socket_path);

        let listener = LocalIpcListener::bind(&endpoint)
            .await
            .expect("listener should bind");

        let server_task = tokio::spawn(async move {
            let mut server_stream = listener.accept().await.expect("accept should work");
            let envelope = server_stream
                .recv_envelope()
                .await
                .expect("receive should work");
            assert_eq!(envelope.kind, EnvelopeKind::Request);
            let request: Request = decode(&envelope.payload).expect("payload should decode");
            assert_eq!(request, Request::Ping);

            let reply_payload = encode(&Request::ServerStatus).expect("reply payload encode");
            let reply = Envelope::new(envelope.request_id, EnvelopeKind::Response, reply_payload);
            server_stream
                .send_envelope(&reply)
                .await
                .expect("send reply should work");
        });

        let mut client_stream = LocalIpcStream::connect(&endpoint)
            .await
            .expect("client should connect");
        let request_payload = encode(&Request::Ping).expect("request payload encode");
        let request = Envelope::new(5, EnvelopeKind::Request, request_payload);
        client_stream
            .send_envelope(&request)
            .await
            .expect("send request should work");
        let response = client_stream
            .recv_envelope()
            .await
            .expect("receive response should work");
        assert_eq!(response.request_id, 5);
        assert_eq!(response.kind, EnvelopeKind::Response);

        server_task.await.expect("server task should finish");
        if socket_path.exists() {
            std::fs::remove_file(&socket_path).expect("socket cleanup should succeed");
        }
    }

    #[tokio::test]
    async fn connect_rejects_wrong_transport_for_platform() {
        #[cfg(unix)]
        let endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");

        #[cfg(windows)]
        let endpoint = IpcEndpoint::unix_socket(std::env::temp_dir().join("bmux-test.sock"));

        let result = LocalIpcStream::connect(&endpoint).await;
        assert!(matches!(
            result,
            Err(IpcTransportError::UnsupportedEndpoint)
        ));
    }
}
