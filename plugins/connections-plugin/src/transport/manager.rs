//! Connection manager for the connecting side: one Iroh endpoint per process
//! (per device key) and one QUIC connection per host, shared by every client in
//! the process. Each `BmuxClient` is one bidirectional stream on it.
//!
//! Iroh runs an endpoint's tasks on the runtime that binds it, and the shared
//! endpoint lives for the process, so it must only be used from the process's
//! host runtime: every plugin entry, and the CLI attach that dials through the
//! saved-host connector, runs there.
use super::{SESSION_ALPN, close, endpoint_addr, erased, path_of};
use crate::config::RelayPolicy;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Idle timeout of session connections.
///
/// Iroh sends a QUIC keep-alive every 5 s on an otherwise idle connection (its
/// default heartbeat), so an idle session costs only those keep-alives on the
/// wire, and three missed in a row mean the path is gone. QUIC's default of
/// 30 s equals the client's request timeout, so a dead path raced the
/// in-flight request; at 15 s the connection (and every stream on it) closes
/// first, which ends an attachment as a drop that reconnects. The effective
/// timeout is the lower of both peers', so the host also releases a vanished
/// client sooner. QUIC migration (a Wi-Fi switch) still resumes within it.
pub const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// How to reach one host.
#[derive(Clone, Debug)]
pub struct HostRoute {
    pub host_key: iroh::PublicKey,
    pub addrs: Vec<SocketAddr>,
    pub relay: Option<String>,
}

/// The connecting side's endpoint settings.
#[derive(Clone, Debug, Default)]
pub struct ClientTransport {
    pub relay: RelayPolicy,
    /// Bind only this address (its IP with any port when the port is taken,
    /// for example by this profile's own hosting listener).
    pub bind_addr: Option<SocketAddr>,
}

pub struct ConnectionManager {
    secret: iroh::SecretKey,
    transport: ClientTransport,
    endpoint: tokio::sync::OnceCell<iroh::Endpoint>,
    connections: tokio::sync::Mutex<HashMap<iroh::PublicKey, iroh::endpoint::Connection>>,
}

impl std::fmt::Debug for ConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionManager")
            .field("device", &self.secret.public())
            .finish_non_exhaustive()
    }
}

type Shared = Mutex<HashMap<iroh::PublicKey, Arc<ConnectionManager>>>;

impl ConnectionManager {
    /// The process-wide manager for this device key.
    pub fn shared(secret: &iroh::SecretKey, transport: &ClientTransport) -> Arc<Self> {
        static MANAGERS: OnceLock<Shared> = OnceLock::new();
        let mut managers = MANAGERS
            .get_or_init(Shared::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(managers.entry(secret.public()).or_insert_with(|| {
            Arc::new(Self {
                secret: secret.clone(),
                transport: transport.clone(),
                endpoint: tokio::sync::OnceCell::new(),
                connections: tokio::sync::Mutex::default(),
            })
        }))
    }

    /// The single endpoint, bound on first use.
    pub async fn endpoint(&self) -> Result<iroh::Endpoint> {
        let secret = self.secret.clone();
        let transport = self.transport.clone();
        self.endpoint
            .get_or_try_init(|| async move {
                let bind = |addr| {
                    let secret = secret.clone();
                    let relay = transport.relay.clone();
                    async move {
                        super::builder(&secret, &relay, addr, &[])?
                            .bind()
                            .await
                            .map_err(anyhow::Error::new)
                    }
                };
                match transport.bind_addr {
                    Some(addr) if addr.port() != 0 => match bind(Some(addr)).await {
                        Ok(endpoint) => Ok(endpoint),
                        Err(_) => bind(Some(SocketAddr::new(addr.ip(), 0))).await,
                    },
                    addr => bind(addr).await,
                }
                .context("start the connecting endpoint")
            })
            .await
            .cloned()
    }

    /// Dial `route` on `alpn` without caching (pairing uses this).
    pub async fn dial(
        &self,
        route: &HostRoute,
        alpn: &'static [u8],
        timeout: Duration,
    ) -> Result<iroh::endpoint::Connection> {
        self.dial_with(route, alpn, timeout, iroh::endpoint::ConnectOptions::new())
            .await
    }

    async fn dial_with(
        &self,
        route: &HostRoute,
        alpn: &'static [u8],
        timeout: Duration,
        options: iroh::endpoint::ConnectOptions,
    ) -> Result<iroh::endpoint::Connection> {
        let endpoint = self.endpoint().await?;
        let addr = endpoint_addr(route.host_key, &route.addrs, route.relay.as_deref())?;
        let connect = async {
            let connecting = endpoint
                .connect_with_opts(addr, alpn, options)
                .await
                .map_err(anyhow::Error::new)?;
            connecting.await.map_err(anyhow::Error::new)
        };
        tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| anyhow::anyhow!("the host did not answer within {}s", timeout.as_secs()))?
            .map_err(|error| error.context("could not reach the host"))
    }

    /// The shared session connection to `route`, dialing when there is none.
    pub async fn connection(
        &self,
        route: &HostRoute,
        timeout: Duration,
    ) -> Result<iroh::endpoint::Connection> {
        let mut connections = self.connections.lock().await;
        if let Some(existing) = connections.get(&route.host_key) {
            if existing.close_reason().is_none() {
                return Ok(existing.clone());
            }
            // Dead: never hand it out again, even if the redial below fails.
            connections.remove(&route.host_key);
        }
        let transport = iroh::endpoint::QuicTransportConfig::builder()
            .max_idle_timeout(Some(SESSION_IDLE_TIMEOUT.try_into()?))
            .build();
        let connection = self
            .dial_with(
                route,
                SESSION_ALPN,
                timeout,
                iroh::endpoint::ConnectOptions::new().with_transport_config(transport),
            )
            .await?;
        connections.insert(route.host_key, connection.clone());
        drop(connections);
        Ok(connection)
    }

    /// One new bidirectional stream to `route`, plus the path in use.
    pub async fn open_stream(
        &self,
        route: &HostRoute,
        timeout: Duration,
    ) -> Result<(bmux_ipc::transport::ErasedIpcStream, &'static str)> {
        for attempt in 0..2 {
            let connection = self.connection(route, timeout).await?;
            let path = path_of(&connection).0;
            match connection.open_bi().await {
                Ok((send, recv)) => return Ok((erased(send, recv), path)),
                Err(error) => {
                    if let Some(refused) = refused(&connection) {
                        return Err(refused.into());
                    }
                    self.forget(route, &connection).await;
                    // A stale cached connection: dial again once.
                    if attempt == 1 {
                        return Err(anyhow::Error::new(error).context("open a stream to the host"));
                    }
                }
            }
        }
        unreachable!("loop returns")
    }

    /// Drop `connection` from the cache (if still cached) so the next use
    /// dials again instead of reusing a connection that failed.
    pub async fn forget(&self, route: &HostRoute, connection: &iroh::endpoint::Connection) {
        let mut connections = self.connections.lock().await;
        if connections
            .get(&route.host_key)
            .is_some_and(|cached| cached.stable_id() == connection.stable_id())
        {
            connections.remove(&route.host_key);
        }
    }

    /// Number of live cached connections.
    #[cfg(test)]
    pub async fn live_connections(&self) -> usize {
        self.connections
            .lock()
            .await
            .values()
            .filter(|connection| connection.close_reason().is_none())
            .count()
    }
}

/// A host's refusal, from the application close code, as a user message.
pub fn refusal(connection: &iroh::endpoint::Connection) -> Option<String> {
    let iroh::endpoint::ConnectionError::ApplicationClosed(close) = connection.close_reason()?
    else {
        return None;
    };
    let code = u32::try_from(close.error_code.into_inner()).ok()?;
    match code {
        close::NOT_PAIRED => Some(
            "the host does not accept this device: it is not paired or was revoked. Pair again with `bmux connect --pair`"
                .to_owned(),
        ),
        close::REVOKED => Some("this device's access was revoked on the host".to_owned()),
        close::STOPPING => Some("the host stopped remote access".to_owned()),
        close::NO_PAIRING => Some(
            "the host has no pairing open; run `bmux remote pair` on the host and use the new link"
                .to_owned(),
        ),
        _ => None,
    }
}

/// A host's refusal of this device, read from the connection's close code.
#[derive(Debug)]
pub struct Refused {
    code: u32,
    message: String,
}

impl Refused {
    /// Whether the host refuses this device itself (never paired, or
    /// revoked): connecting again cannot succeed until it is paired again.
    #[must_use]
    pub const fn is_permanent(&self) -> bool {
        matches!(self.code, close::NOT_PAIRED | close::REVOKED)
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Refused {}

/// The host's refusal, if `connection` was closed with one.
pub fn refused(connection: &iroh::endpoint::Connection) -> Option<Refused> {
    let message = refusal(connection)?;
    let iroh::endpoint::ConnectionError::ApplicationClosed(close) = connection.close_reason()?
    else {
        return None;
    };
    let code = u32::try_from(close.error_code.into_inner()).ok()?;
    Some(Refused { code, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn one_manager_and_endpoint_per_device_key() {
        let secret = iroh::SecretKey::generate();
        let transport = ClientTransport {
            relay: RelayPolicy::Disabled,
            bind_addr: Some("127.0.0.1:0".parse().unwrap()),
        };
        let first = ConnectionManager::shared(&secret, &transport);
        let second = ConnectionManager::shared(&secret, &transport);
        assert!(Arc::ptr_eq(&first, &second));
        let endpoint = first.endpoint().await.unwrap();
        assert_eq!(endpoint.id(), secret.public());
        assert!(
            endpoint
                .bound_sockets()
                .iter()
                .all(|addr| addr.ip().is_loopback())
        );
        assert_eq!(
            second.endpoint().await.unwrap().bound_sockets(),
            endpoint.bound_sockets()
        );
        assert!(!Arc::ptr_eq(
            &first,
            &ConnectionManager::shared(&iroh::SecretKey::generate(), &transport)
        ));
        assert_eq!(first.live_connections().await, 0);
    }
}
