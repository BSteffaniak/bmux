//! Named-pipe client acquisition is an immediate operation in Tokio. Expose its
//! ready result as a future so the common async API need not fake a suspension,
//! spawn blocking work, or introduce a new busy-pipe retry policy.
use super::{IdentityPolicy, IpcEndpoint, IpcTransportError, LocalIpcStream, StreamInner};
use std::future::{Ready, ready};
use std::io;
use tokio::net::windows::named_pipe::ClientOptions;

pub(super) fn connect(
    endpoint: &IpcEndpoint,
    peer: IdentityPolicy,
) -> Ready<Result<LocalIpcStream, IpcTransportError>> {
    ready(connect_now(endpoint, peer))
}

fn connect_now(
    endpoint: &IpcEndpoint,
    peer: IdentityPolicy,
) -> Result<LocalIpcStream, IpcTransportError> {
    if peer != IdentityPolicy::EffectiveUser {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix peer policy on non-Unix platform",
        )
        .into());
    }
    if let IpcEndpoint::WindowsNamedPipe(pipe_name) = endpoint {
        let stream = ClientOptions::new().open(pipe_name)?;
        return Ok(LocalIpcStream {
            inner: StreamInner::WindowsClient(stream),
        });
    }
    Err(IpcTransportError::UnsupportedEndpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_policy_is_rejected_before_open() {
        let endpoint =
            IpcEndpoint::WindowsNamedPipe(r"\\.\pipe\bmux-nonexistent-policy-test".into());
        let error = connect(&endpoint, IdentityPolicy::Unrestricted)
            .await
            .unwrap_err();
        assert!(
            matches!(error, IpcTransportError::Io(error) if error.kind() == io::ErrorKind::Unsupported)
        );
    }
}
