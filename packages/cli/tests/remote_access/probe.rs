//! A minimal Iroh dialer standing in for an unpaired internet peer: it knows the host's
//! endpoint id and direct address, has its own fresh device key, and nothing else.

use std::net::SocketAddr;
use std::time::Duration;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};

pub const SESSION_ALPN: &[u8] = b"bmux/session/1";
pub const PAIR_ALPN: &[u8] = b"bmux/pair/1";

/// What a peer got from one dial.
#[derive(Debug)]
pub enum Dial {
    /// The handshake failed: the host refused before any application stream existed.
    Refused(String),
    /// The handshake completed; what the peer then observed.
    Connected(AfterHandshake),
}

/// What happened after a completed handshake.
#[derive(Debug)]
pub struct AfterHandshake {
    /// Bytes the host sent back on the peer's stream.
    pub reply_bytes: usize,
    /// Whether the host closed the connection within the observation window.
    pub closed: bool,
    /// How long until the host closed it.
    pub closed_after: Option<Duration>,
}

/// An unpaired peer with a fresh key.
pub struct Peer {
    runtime: tokio::runtime::Runtime,
    endpoint: Endpoint,
}

impl Peer {
    #[must_use]
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("probe runtime");
        let endpoint = runtime.block_on(async {
            Endpoint::builder(presets::N0)
                .secret_key(SecretKey::generate())
                .clear_address_lookup()
                .clear_relay_transports()
                .clear_ip_transports()
                .bind_addr("127.0.0.1:0")
                .expect("probe bind address")
                .bind()
                .await
                .expect("bind probe endpoint")
        });
        Self { runtime, endpoint }
    }

    /// Dials `host` at `addr` with `alpn`, then (if admitted) sends `payload` on one stream
    /// and watches the connection for `window`.
    pub fn dial(
        &self,
        host: EndpointId,
        addr: SocketAddr,
        alpn: &[u8],
        payload: &[u8],
        window: Duration,
    ) -> Dial {
        self.runtime.block_on(async {
            let target = EndpointAddr::new(host).with_ip_addr(addr);
            let connection = match tokio::time::timeout(
                Duration::from_secs(15),
                self.endpoint.connect(target, alpn),
            )
            .await
            {
                Err(_) => return Dial::Refused("handshake timed out".into()),
                Ok(Err(error)) => return Dial::Refused(error.to_string()),
                Ok(Ok(connection)) => connection,
            };
            let started = tokio::time::Instant::now();
            let mut reply_bytes = 0usize;
            let exchange = async {
                let (mut send, mut recv) = connection.open_bi().await.ok()?;
                // Large payloads may be cut off by the host; that is the point.
                let _ = send.write_all(payload).await;
                let _ = send.finish();
                let mut buffer = vec![0u8; 16 * 1024];
                while let Ok(Some(read)) = recv.read(&mut buffer).await {
                    reply_bytes += read;
                }
                Some(())
            };
            let _ = tokio::time::timeout(window, exchange).await;
            let closed = tokio::time::timeout(
                window.saturating_sub(started.elapsed()),
                connection.closed(),
            )
            .await
            .is_ok();
            let after = AfterHandshake {
                reply_bytes,
                closed,
                closed_after: closed.then(|| started.elapsed()),
            };
            connection.close(0u32.into(), b"probe done");
            Dial::Connected(after)
        })
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.runtime.block_on(self.endpoint.close());
    }
}

/// Requires that a dial was refused: during the handshake, or by the host closing the
/// connection without ever answering on a stream.
pub fn assert_refused(dial: &Dial, what: &str, window: Duration) {
    match dial {
        Dial::Refused(reason) => eprintln!("{what}: refused during the handshake: {reason}"),
        Dial::Connected(after) => {
            assert_eq!(after.reply_bytes, 0, "{what}: the host answered: {after:?}");
            assert!(
                after.closed && after.closed_after.is_some_and(|t| t <= window),
                "{what}: the host kept the connection open: {after:?}"
            );
        }
    }
}
