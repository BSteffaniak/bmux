//! Purpose-named standalone presentation networking. TLS is the remote transport;
//! plaintext is available only on an explicitly supplied loopback listener.
use crate::{Host, HostError, InteractionMode, PROTOCOL_VERSION, Request, Response};
use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};

pub type SharedHost = Arc<Mutex<Host>>;

#[derive(Clone, Debug)]
pub struct NetworkConfig {
    pub max_connections: usize,
    pub handshake_timeout: Duration,
    pub request_timeout: Duration,
    pub response_timeout: Duration,
}
impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            max_connections: 64,
            handshake_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            response_timeout: Duration::from_secs(5),
        }
    }
}

/// Caller supplies a validated certificate/private key configuration. No plaintext fallback.
pub async fn serve_tls(
    listener: TcpListener,
    host: SharedHost,
    tls: Arc<rustls::ServerConfig>,
    config: NetworkConfig,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    serve(
        listener,
        host,
        Some(TlsAcceptor::from(tls)),
        config,
        shutdown,
    )
    .await
}

pub async fn serve_local(
    listener: TcpListener,
    host: SharedHost,
    config: NetworkConfig,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "plaintext presentation listeners must be loopback",
        ));
    }
    serve(listener, host, None, config, shutdown).await
}

async fn serve(
    listener: TcpListener,
    host: SharedHost,
    tls: Option<TlsAcceptor>,
    config: NetworkConfig,
    mut shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    if config.max_connections == 0
        || config.max_connections > 1024
        || config.handshake_timeout.is_zero()
        || config.request_timeout.is_zero()
        || config.response_timeout.is_zero()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid network limits",
        ));
    }
    let slots = Arc::new(Semaphore::new(config.max_connections));
    let mut connections = JoinSet::new();
    let result = loop {
        if *shutdown.borrow() {
            break Ok(());
        }
        tokio::select! {
            changed = shutdown.changed() => { if changed.is_err() || *shutdown.borrow() { break Ok(()); } }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                let (socket, peer) = match accepted { Ok(value) => value, Err(error) => break Err(error) };
                if tls.is_none() && !peer.ip().is_loopback() { continue; }
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue; };
                let host = host.clone(); let tls = tls.clone(); let config = config.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    if let Some(tls) = tls {
                        if let Ok(Ok(stream)) = timeout(config.handshake_timeout, tls.accept(socket)).await {
                            let _result = connection(stream, host, config).await;
                        }
                    } else { let _result = connection(socket, host, config).await; }
                });
            }
        }
    };
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    result
}

struct ConnectionParticipant {
    host: SharedHost,
    participant: Option<String>,
}
impl Drop for ConnectionParticipant {
    fn drop(&mut self) {
        if let Some(id) = &self.participant
            && let Ok(mut host) = self.host.lock()
        {
            host.disconnect(id);
        }
    }
}
fn locked(host: &SharedHost) -> io::Result<std::sync::MutexGuard<'_, Host>> {
    host.lock()
        .map_err(|_| io::Error::other("presentation host unavailable"))
}
async fn connection<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    host: SharedHost,
    config: NetworkConfig,
) -> io::Result<()> {
    let limit = locked(&host)?.limits().message_bytes;
    let mut session = ConnectionParticipant {
        host,
        participant: None,
    };
    loop {
        let bytes = timeout(config.request_timeout, read_message(&mut stream, limit))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "presentation request timeout")
            })??;
        let request: Request =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid presentation request"))?;
        let response = {
            let mut host = locked(&session.host)?;
            host.handle(&mut session.participant, request)
        };
        let fatal = matches!(
            response,
            Response::Error(HostError::Authentication | HostError::Version)
        );
        timeout(
            config.response_timeout,
            write_message(&mut stream, &response, limit),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "presentation response timeout"))??;
        if fatal {
            return Ok(());
        }
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

async fn read_message<S: AsyncRead + Unpin>(stream: &mut S, limit: usize) -> io::Result<Vec<u8>> {
    let length =
        usize::try_from(stream.read_u32().await?).map_err(|_| invalid("invalid message length"))?;
    if length == 0 || length > limit {
        return Err(invalid("presentation message exceeds limit"));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    Ok(bytes)
}
struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(invalid("presentation message exceeds limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
async fn write_message<S: AsyncWrite + Unpin, T: serde::Serialize>(
    stream: &mut S,
    value: &T,
    limit: usize,
) -> io::Result<()> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| invalid("presentation message cannot be encoded within limit"))?;
    let length = u32::try_from(writer.bytes.len())
        .map_err(|_| invalid("presentation message exceeds wire length"))?;
    stream.write_u32(length).await?;
    stream.write_all(&writer.bytes).await?;
    stream.flush().await
}

trait ParticipantStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ParticipantStream for T {}

/// Single in-flight request; no retries or hidden reconnect/replay. A timeout poisons
/// the connection because an effect may have committed before its reply was lost.
pub struct Client {
    stream: Box<dyn ParticipantStream>,
    limit: usize,
    timeout: Duration,
    poisoned: bool,
}
impl Client {
    pub async fn connect_tls(
        address: SocketAddr,
        server_name: String,
        tls: Arc<rustls::ClientConfig>,
        limit: usize,
        deadline: Duration,
    ) -> io::Result<Self> {
        validate_client_limits(limit, deadline)?;
        let name = rustls::pki_types::ServerName::try_from(server_name)
            .map_err(|_| invalid("invalid TLS server name"))?;
        let stream = timeout(deadline, async {
            let socket = TcpStream::connect(address).await?;
            TlsConnector::from(tls).connect(name, socket).await
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "presentation TLS connection timeout",
            )
        })??;
        Ok(Self {
            stream: Box::new(stream),
            limit,
            timeout: deadline,
            poisoned: false,
        })
    }
    pub async fn connect_local(
        address: SocketAddr,
        limit: usize,
        deadline: Duration,
    ) -> io::Result<Self> {
        validate_client_limits(limit, deadline)?;
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "plaintext presentation connections must be loopback",
            ));
        }
        let stream = timeout(deadline, TcpStream::connect(address))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "presentation connection timeout")
            })??;
        Ok(Self {
            stream: Box::new(stream),
            limit,
            timeout: deadline,
            poisoned: false,
        })
    }
    pub async fn authenticate(
        &mut self,
        credential: Option<String>,
        mode: InteractionMode,
    ) -> io::Result<Response> {
        self.request(Request::Authenticate {
            version: PROTOCOL_VERSION,
            credential,
            mode,
        })
        .await
    }
    pub async fn request(&mut self, request: Request) -> io::Result<Response> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "reconnect and reauthenticate; never replay uncertain effects",
            ));
        }
        // Remains poisoned if the caller cancels this future mid-request.
        self.poisoned = true;
        let result: io::Result<Response> = timeout(self.timeout, async {
            write_message(&mut self.stream, &request, self.limit).await?;
            let bytes = read_message(&mut self.stream, self.limit).await?;
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid presentation response"))
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "presentation request outcome uncertain",
            )
        })?;
        if let Ok(Response::Poll(crate::Poll::Snapshot { snapshot, .. })) = &result {
            let limits = crate::Limits::default();
            if snapshot.surfaces.len() > limits.surfaces {
                return Err(invalid("too many mirror surfaces"));
            }
            for frame in &snapshot.surfaces {
                frame
                    .validate(limits.canvas_cells, limits.frame_bytes)
                    .map_err(|_| invalid("invalid mirror scene"))?;
            }
        }
        if result.is_ok() {
            self.poisoned = false;
        }
        result
    }
}
/// Parse bounded caller-provided PEM material. The application owns file permissions
/// and trust selection; this helper neither generates trust nor disables verification.
pub fn tls_server_config(
    certificates_pem: &[u8],
    key_pem: &[u8],
) -> io::Result<Arc<rustls::ServerConfig>> {
    if certificates_pem.len() > 256 * 1024 || key_pem.len() > 64 * 1024 {
        return Err(invalid("TLS material exceeds limit"));
    }
    let certificates = rustls_pemfile::certs(&mut std::io::Cursor::new(certificates_pem))
        .collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() || certificates.len() > 16 {
        return Err(invalid("invalid certificate chain"));
    }
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(key_pem))?
        .ok_or_else(|| invalid("private key required"))?;
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| invalid("TLS protocol configuration failed"))?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(|_| invalid("TLS certificate configuration failed"))?;
    Ok(Arc::new(config))
}
/// Explicit caller-selected trust roots; rustls verifies both chain and server name.
pub fn tls_client_config(roots_pem: &[u8]) -> io::Result<Arc<rustls::ClientConfig>> {
    if roots_pem.len() > 256 * 1024 {
        return Err(invalid("TLS trust material exceeds limit"));
    }
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(roots_pem)) {
        roots
            .add(cert?)
            .map_err(|_| invalid("invalid trust certificate"))?;
    }
    if roots.is_empty() {
        return Err(invalid("TLS trust roots required"));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| invalid("TLS protocol configuration failed"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(Arc::new(config))
}

fn validate_client_limits(limit: usize, deadline: Duration) -> io::Result<()> {
    if limit == 0 || limit > 64 * 1024 * 1024 || deadline.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid presentation client limits",
        ));
    }
    Ok(())
}
