use crate::frame::{FrameDecodeError, FrameEncodeError, decode_frame_exact, encode_frame};
use crate::{Envelope, IpcEndpoint};
use std::path::Path;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Local IPC listener abstraction.
#[derive(Debug)]
pub struct LocalIpcListener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
}

/// Local IPC stream abstraction.
#[derive(Debug)]
pub struct LocalIpcStream {
    #[cfg(unix)]
    inner: tokio::net::UnixStream,
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
                return Ok(Self { inner: listener });
            }
            return Err(IpcTransportError::UnsupportedEndpoint);
        }

        #[cfg(not(unix))]
        {
            let _ = endpoint;
            Err(IpcTransportError::UnsupportedEndpoint)
        }
    }

    /// Accept an incoming local connection.
    ///
    /// # Errors
    ///
    /// Returns an error when accepting fails.
    pub async fn accept(&self) -> Result<LocalIpcStream, IpcTransportError> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            return Ok(LocalIpcStream { inner: stream });
        }

        #[cfg(not(unix))]
        {
            Err(IpcTransportError::UnsupportedEndpoint)
        }
    }
}

impl LocalIpcStream {
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
                return Ok(Self { inner: stream });
            }
            return Err(IpcTransportError::UnsupportedEndpoint);
        }

        #[cfg(not(unix))]
        {
            let _ = endpoint;
            Err(IpcTransportError::UnsupportedEndpoint)
        }
    }

    /// Send a single framed envelope.
    ///
    /// # Errors
    ///
    /// Returns an error if frame encoding or socket writes fail.
    pub async fn send_envelope(&mut self, envelope: &Envelope) -> Result<(), IpcTransportError> {
        let frame = encode_frame(envelope)?;

        #[cfg(unix)]
        {
            self.inner.write_all(&frame).await?;
            self.inner.flush().await?;
            return Ok(());
        }

        #[cfg(not(unix))]
        {
            let _ = frame;
            Err(IpcTransportError::UnsupportedEndpoint)
        }
    }

    /// Receive a single framed envelope.
    ///
    /// # Errors
    ///
    /// Returns an error if frame reads fail or the frame is invalid.
    pub async fn recv_envelope(&mut self) -> Result<Envelope, IpcTransportError> {
        #[cfg(unix)]
        {
            let mut len_bytes = [0_u8; 4];
            self.inner.read_exact(&mut len_bytes).await?;
            let payload_len = u32::from_le_bytes(len_bytes) as usize;
            let mut frame = Vec::with_capacity(4 + payload_len);
            frame.extend_from_slice(&len_bytes);
            frame.resize(4 + payload_len, 0);
            self.inner.read_exact(&mut frame[4..]).await?;
            let envelope = decode_frame_exact(&frame)?;
            return Ok(envelope);
        }

        #[cfg(not(unix))]
        {
            Err(IpcTransportError::UnsupportedEndpoint)
        }
    }
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
        let endpoint = IpcEndpoint::windows_named_pipe(r"\\.\pipe\bmux-test");
        let result = LocalIpcStream::connect(&endpoint).await;
        assert!(matches!(result, Err(IpcTransportError::UnsupportedEndpoint)));
    }
}
