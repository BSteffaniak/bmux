//! Iroh (QUIC) transport: one endpoint per process for connecting, one
//! listener per hosting runtime, and the two ALPNs.
pub mod listener;
pub mod manager;

use crate::config::RelayPolicy;
use anyhow::Result;
use std::net::SocketAddr;
use std::time::Duration;

/// Paired devices' bmux streams. The peer key must be an active device.
pub const SESSION_ALPN: &[u8] = b"bmux/session/1";
/// The pairing exchange, accepted only while a pairing session is open.
pub const PAIR_ALPN: &[u8] = b"bmux/pair/1";

/// Application close codes on QUIC connections.
pub mod close {
    /// The device key is not an active paired device.
    pub const NOT_PAIRED: u32 = 401;
    /// The device was revoked while connected.
    pub const REVOKED: u32 = 403;
    /// No pairing session is open.
    pub const NO_PAIRING: u32 = 404;
    /// Hosting stopped.
    pub const STOPPING: u32 = 503;
    /// Normal close.
    pub const DONE: u32 = 0;
}

/// An endpoint builder for `secret` under the relay policy, bound only to
/// `bind_addr` when set, advertising `external_addrs`.
pub fn builder(
    secret: &iroh::SecretKey,
    relay: &RelayPolicy,
    bind_addr: Option<SocketAddr>,
    external_addrs: &[SocketAddr],
) -> Result<iroh::endpoint::Builder> {
    let builder = iroh::Endpoint::builder(iroh::endpoint::presets::N0).secret_key(secret.clone());
    let mut builder = relay.apply(builder)?;
    if let Some(addr) = bind_addr {
        builder = builder.clear_ip_transports().bind_addr(addr)?;
    }
    for addr in external_addrs {
        builder = builder.external_addr(*addr);
    }
    Ok(builder)
}

/// Where to dial a host: its pinned key plus address and relay hints.
pub fn endpoint_addr(
    host_key: iroh::PublicKey,
    addrs: &[SocketAddr],
    relay: Option<&str>,
) -> Result<iroh::EndpointAddr> {
    let mut addr = iroh::EndpointAddr::new(host_key);
    for direct in addrs {
        addr = addr.with_ip_addr(*direct);
    }
    if let Some(relay) = relay {
        addr = addr.with_relay_url(relay.parse()?);
    }
    Ok(addr)
}

/// `direct` or `relay` for the selected path, with its RTT.
pub fn path_of(connection: &iroh::endpoint::Connection) -> (&'static str, Option<Duration>) {
    connection
        .paths()
        .iter()
        .find(iroh::endpoint::Path::is_selected)
        .map_or(("unknown", None), |path| {
            (
                if path.is_relay() { "relay" } else { "direct" },
                Some(path.rtt()),
            )
        })
}

/// One QUIC bidirectional stream as an `ErasedIpcStream`.
pub fn erased(
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) -> bmux_ipc::transport::ErasedIpcStream {
    bmux_ipc::transport::ErasedIpcStream::new(Box::new(tokio::io::join(recv, send)))
}
