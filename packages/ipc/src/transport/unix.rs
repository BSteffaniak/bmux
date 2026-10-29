//! Unix connection and authenticated peer verification.
use super::{
    IdentityPolicy, IpcEndpoint, IpcTransportError, LocalIpcStream, StreamInner, unix_security,
};

pub(super) async fn connect(
    endpoint: &IpcEndpoint,
    peer: IdentityPolicy,
) -> Result<LocalIpcStream, IpcTransportError> {
    if let IpcEndpoint::UnixSocket(path) = endpoint {
        let stream = tokio::net::UnixStream::connect(path).await?;
        unix_security::verify_peer_with_policy(&stream, peer)?;
        return Ok(LocalIpcStream {
            inner: StreamInner::Unix(stream),
        });
    }
    Err(IpcTransportError::UnsupportedEndpoint)
}
