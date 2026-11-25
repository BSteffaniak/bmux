//! A local Iroh relay for the relay-only gate.
//!
//! It serves plain HTTP on 127.0.0.1. Relay TLS only protects the hop to the relay: what
//! passes through it is QUIC between the two pinned endpoint keys, end-to-end encrypted. A
//! loopback `http://` relay URL therefore needs no certificate trust and no switch that
//! weakens verification for real relays (bmux accepts `http://` relay URLs only for loopback
//! IP literals; every other custom relay must be `https://`).
//!
//! The relay records which endpoint ids it admitted, so a test can prove that traffic
//! actually went through it.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh_relay::server::{Access, AccessControl, ClientRequest, RelayConfig, Server, ServerConfig};

#[derive(Debug, Default)]
struct Recorder {
    admitted: Mutex<BTreeSet<String>>,
}

impl AccessControl for Recorder {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        self.admitted
            .lock()
            .expect("relay admissions")
            .insert(request.endpoint_id().to_string());
        Access::Allow
    }
}

/// A running relay; stops when dropped.
pub struct LocalRelay {
    url: String,
    recorder: Arc<Recorder>,
    // Dropped first: it shuts the server down on the runtime below.
    server: Option<Server>,
    runtime: tokio::runtime::Runtime,
}

impl LocalRelay {
    #[must_use]
    pub fn start() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("relay runtime");
        let recorder = Arc::new(Recorder::default());
        let server = runtime.block_on(async {
            let mut relay = RelayConfig::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
            relay.access = recorder.clone();
            let mut config = ServerConfig::default();
            config.relay = Some(relay);
            Server::spawn(config).await.expect("spawn local relay")
        });
        let addr = server.http_addr().expect("relay serves HTTP");
        Self {
            url: format!("http://{addr}"),
            recorder,
            server: Some(server),
            runtime,
        }
    }

    /// The relay URL for `[...remote.relay] urls`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Endpoint ids the relay admitted so far.
    #[must_use]
    pub fn admitted(&self) -> BTreeSet<String> {
        self.recorder
            .admitted
            .lock()
            .expect("relay admissions")
            .clone()
    }

    /// Whether the relay is serving: an HTTP request to it gets an answer.
    #[must_use]
    pub fn is_serving(&self) -> bool {
        use std::io::{Read as _, Write as _};
        let addr: SocketAddr = self
            .url
            .trim_start_matches("http://")
            .parse()
            .expect("relay address");
        let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        if stream
            .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .is_err()
        {
            return false;
        }
        let mut answer = Vec::new();
        let _ = stream.read_to_end(&mut answer);
        answer.starts_with(b"HTTP/1.1")
    }
}

impl Drop for LocalRelay {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            let _ = self.runtime.block_on(server.shutdown());
        }
    }
}
