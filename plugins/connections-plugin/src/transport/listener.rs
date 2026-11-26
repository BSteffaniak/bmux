//! The host listener. `bmux/session/1` connections from active devices hand
//! each bidirectional stream straight to the server (B1 `StreamIngressHandle`)
//! with one revocable binding per connection; anything else is closed right
//! after the handshake, before any bmux bytes are read. `bmux/pair/1` is
//! served only while a pairing session is open.
use super::{PAIR_ALPN, SESSION_ALPN, close, path_of};
use crate::audit::{Event as AuditEvent, Kind as AuditKind, RefusalLimiter};
use crate::pairing::protocol::{self, DeviceMessage, HostMessage, Refusal};
use crate::pairing::{Decision, Sessions};
use crate::trust::{DeviceState, TrustChange, TrustStore};
use anyhow::{Context, Result};
use bmux_plugin::stream_admission::{
    StreamAdmissionBinding, StreamIngressError, StreamIngressHandle, StreamRevocation,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::task::JoinSet;
use uuid::Uuid;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Live session connections, every device together.
const MAX_CONNECTIONS: usize = 64;
/// QUIC handshakes in flight: the only work an unpaired peer can cause. They
/// are bounded apart from live sessions, so stalled handshakes never take a
/// paired device's slot.
const MAX_HANDSHAKES: usize = 16;
/// Handshakes in flight from one source (an IP address, or a relayed peer's
/// endpoint id), so one peer cannot hold every handshake slot.
const MAX_HANDSHAKES_PER_SOURCE: usize = 4;
const MAX_PAIRING_CONNECTIONS: usize = 8;
/// Bidirectional streams a peer may have open on one connection. Each is one
/// server client; an attach uses three. Enforced by QUIC flow control, so a
/// client opening more waits instead of failing.
const MAX_STREAMS_PER_CONNECTION: u32 = 32;
/// Remote streams served at once, every device together. The server's
/// connection limit is shared with local clients, so remote streams never
/// take all of it: past this, a new remote stream is reset.
pub const MAX_REMOTE_STREAMS: usize = 40;
/// Bytes a peer may have unread in flight to the host on one connection.
const RECEIVE_WINDOW: u32 = 8 * 1024 * 1024;

/// The host's QUIC limits: no unidirectional streams (bmux uses none), a
/// bounded number of bidirectional ones, and a bounded receive window, so a
/// peer (paired or, during a pairing window, not) cannot make the host buffer
/// without bound.
fn host_transport() -> iroh::endpoint::QuicTransportConfig {
    use iroh::endpoint::VarInt;
    iroh::endpoint::QuicTransportConfig::builder()
        .max_concurrent_bidi_streams(VarInt::from_u32(MAX_STREAMS_PER_CONNECTION))
        .max_concurrent_uni_streams(VarInt::from_u32(0))
        .receive_window(VarInt::from_u32(RECEIVE_WINDOW))
        .build()
}

#[derive(Debug)]
pub struct BindFailure(pub anyhow::Error);

impl std::fmt::Display for BindFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("remote listener bind failed")
    }
}

impl std::error::Error for BindFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// What the listener needs from the host.
#[derive(Clone)]
pub struct ListenerContext {
    pub trust: Arc<TrustStore>,
    pub sessions: Arc<Sessions>,
    pub ingress: StreamIngressHandle,
    /// The host key; pairing transcripts bind it.
    pub host_key: iroh::PublicKey,
    /// Bounds refusals written to the audit log.
    pub refusals: Arc<RefusalLimiter>,
}

/// Append `event` to the audit log off the runtime workers (a redb commit).
fn audit(trust: &Arc<TrustStore>, event: AuditEvent) {
    let trust = Arc::clone(trust);
    drop(bmux_plugin_sdk::rt::spawn_blocking_named(
        "remote-audit",
        move || trust.record(&event),
    ));
}

struct LiveConnection {
    device_key: String,
    device_name: String,
    connection: iroh::endpoint::Connection,
    revocation: StreamRevocation,
    since: Instant,
}

/// Live session connections by connection id.
#[derive(Default)]
pub struct LiveRegistry {
    connections: Mutex<HashMap<Uuid, LiveConnection>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveSummary {
    pub device_key: String,
    pub device_name: String,
    pub path: &'static str,
    pub rtt: Option<Duration>,
    pub connected_secs: u64,
}

impl LiveRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, LiveConnection>> {
        self.connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// End every live connection of `device_key` now.
    pub fn revoke_device(&self, device_key: &str, code: u32, reason: &'static [u8]) -> usize {
        let mut connections = self.lock();
        let ids: Vec<_> = connections
            .iter()
            .filter(|(_, live)| live.device_key == device_key)
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(live) = connections.remove(id) {
                live.revocation.revoke();
                live.connection.close(code.into(), reason);
            }
        }
        ids.len()
    }

    /// End every live connection whose device is no longer active.
    pub fn revalidate(&self, trust: &TrustStore) {
        let keys: Vec<_> = self
            .lock()
            .values()
            .map(|live| live.device_key.clone())
            .collect();
        for key in keys {
            if trust.active_principal(&key).is_none() {
                self.revoke_device(&key, close::REVOKED, b"device revoked");
            }
        }
    }

    fn revoke_all(&self, code: u32, reason: &'static [u8]) {
        for (_, live) in self.lock().drain() {
            live.revocation.revoke();
            live.connection.close(code.into(), reason);
        }
    }

    pub fn summaries(&self) -> Vec<LiveSummary> {
        let mut summaries: Vec<_> = self
            .lock()
            .values()
            .map(|live| {
                let (path, rtt) = path_of(&live.connection);
                LiveSummary {
                    device_key: live.device_key.clone(),
                    device_name: live.device_name.clone(),
                    path,
                    rtt,
                    connected_secs: live.since.elapsed().as_secs(),
                }
            })
            .collect();
        summaries.sort_by(|a, b| a.device_name.cmp(&b.device_name));
        summaries
    }
}

pub struct Listener {
    endpoint: iroh::Endpoint,
    live: Arc<LiveRegistry>,
    stop: tokio::sync::watch::Sender<bool>,
    tasks: Option<bmux_plugin_sdk::rt::JoinHandle<()>>,
}

impl Listener {
    /// Bind and start accepting. `builder` carries the host key.
    pub async fn start(builder: iroh::endpoint::Builder, context: ListenerContext) -> Result<Self> {
        let endpoint = builder
            .alpns(vec![SESSION_ALPN.to_vec()])
            .transport_config(host_transport())
            .bind()
            .await
            .map_err(|error| BindFailure(anyhow::Error::new(error)))?;
        let live = Arc::new(LiveRegistry::default());
        let (stop, stopped) = tokio::sync::watch::channel(false);
        // On the host runtime, in the activation scope when started from it.
        let tasks = bmux_plugin_sdk::rt::spawn_named(
            "remote-listener",
            run(endpoint.clone(), context, Arc::clone(&live), stopped),
        );
        Ok(Self {
            endpoint,
            live,
            stop,
            tasks: Some(tasks),
        })
    }

    pub const fn endpoint(&self) -> &iroh::Endpoint {
        &self.endpoint
    }

    pub fn live(&self) -> &LiveRegistry {
        &self.live
    }

    /// Accept `bmux/pair/1` only while a pairing session is open.
    pub fn set_pairing(&self, open: bool) {
        let mut alpns = vec![SESSION_ALPN.to_vec()];
        if open {
            alpns.push(PAIR_ALPN.to_vec());
        }
        self.endpoint.set_alpns(alpns);
    }

    /// Close every connection and the endpoint.
    pub async fn stop(&mut self) {
        self.stop.send_replace(true);
        self.live
            .revoke_all(close::STOPPING, b"remote access stopped");
        if let Some(tasks) = self.tasks.take() {
            let _ = tasks.await;
        }
        self.endpoint.close().await;
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        self.live
            .revoke_all(close::STOPPING, b"remote access stopped");
        if let Some(tasks) = self.tasks.take() {
            tasks.abort();
        }
    }
}

/// Where a handshake comes from, for the per-source bound.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source {
    Ip(std::net::IpAddr),
    Relayed(iroh::EndpointId),
    Other,
}

impl Source {
    fn of(incoming: &iroh::endpoint::Incoming) -> Self {
        match incoming.remote_addr() {
            iroh::endpoint::IncomingAddr::Ip(addr) => Self::Ip(addr.ip()),
            iroh::endpoint::IncomingAddr::Relay { endpoint_id, .. } => Self::Relayed(endpoint_id),
            _ => Self::Other,
        }
    }
}

/// Handshakes in flight, in total and per source.
#[derive(Default)]
struct Handshakes {
    by_source: Mutex<HashMap<Source, usize>>,
}

/// One admitted handshake; dropping it frees its slots.
struct HandshakeSlot {
    handshakes: Arc<Handshakes>,
    source: Source,
}

impl Handshakes {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Source, usize>> {
        self.by_source
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A slot for a handshake from `source`, or `None` when the total or the
    /// source's bound is reached.
    fn admit(self: &Arc<Self>, source: Source) -> Option<HandshakeSlot> {
        let mut by_source = self.lock();
        let total: usize = by_source.values().sum();
        let count = by_source.entry(source.clone()).or_default();
        if total >= MAX_HANDSHAKES || *count >= MAX_HANDSHAKES_PER_SOURCE {
            if *count == 0 {
                by_source.remove(&source);
            }
            return None;
        }
        *count += 1;
        drop(by_source);
        Some(HandshakeSlot {
            handshakes: Arc::clone(self),
            source,
        })
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        let mut by_source = self.handshakes.lock();
        if let Some(count) = by_source.get_mut(&self.source) {
            *count -= 1;
            if *count == 0 {
                by_source.remove(&self.source);
            }
        }
    }
}

/// One remote stream holding a share of [`MAX_REMOTE_STREAMS`] until the
/// server drops it.
struct CountedStream<S> {
    inner: S,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for CountedStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for CountedStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Limits shared by every connection of one listener.
#[derive(Clone)]
struct Limits {
    pairing: Arc<tokio::sync::Semaphore>,
    sessions: Arc<tokio::sync::Semaphore>,
    streams: Arc<tokio::sync::Semaphore>,
}

async fn run(
    endpoint: iroh::Endpoint,
    context: ListenerContext,
    live: Arc<LiveRegistry>,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let mut changes = context.trust.subscribe();
    let limits = Limits {
        pairing: Arc::new(tokio::sync::Semaphore::new(MAX_PAIRING_CONNECTIONS)),
        sessions: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
        streams: Arc::new(tokio::sync::Semaphore::new(MAX_REMOTE_STREAMS)),
    };
    let handshakes = Arc::new(Handshakes::default());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = stopped.changed() => break,
            change = changes.recv() => match change {
                Ok(TrustChange::Revoked { key }) => {
                    live.revoke_device(&key, close::REVOKED, b"device revoked");
                }
                // A role change applies to every later request through the
                // authorizer; the connection keeps running.
                Ok(TrustChange::RoleChanged { .. } | TrustChange::Activated { .. }) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    live.revalidate(&context.trust);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            incoming = endpoint.accept(), if connections.len() < MAX_CONNECTIONS + MAX_HANDSHAKES => {
                let Some(incoming) = incoming else { break };
                // Refusing costs one packet; a stalled handshake would hold a slot.
                let Some(slot) = handshakes.admit(Source::of(&incoming)) else {
                    incoming.refuse();
                    continue;
                };
                let context = context.clone();
                let live = Arc::clone(&live);
                let limits = limits.clone();
                let stopped = stopped.clone();
                connections.spawn(async move {
                    let _ = accept(incoming, slot, context, live, limits, stopped).await;
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
}

async fn accept(
    incoming: iroh::endpoint::Incoming,
    handshake: HandshakeSlot,
    context: ListenerContext,
    live: Arc<LiveRegistry>,
    limits: Limits,
    stopped: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming)
        .await
        .context("handshake timed out")??;
    drop(handshake);
    let alpn = connection.alpn().to_vec();
    if alpn == SESSION_ALPN {
        let Ok(_slot) = limits.sessions.try_acquire() else {
            connection.close(close::BUSY.into(), b"too many connections");
            return Ok(());
        };
        serve_session(connection, &context, &live, &limits.streams, stopped).await;
    } else if alpn == PAIR_ALPN {
        let Ok(_slot) = limits.pairing.try_acquire() else {
            connection.close(close::NO_PAIRING.into(), b"too many pairing connections");
            return Ok(());
        };
        if !context.sessions.is_open() {
            connection.close(close::NO_PAIRING.into(), b"no pairing session");
            return Ok(());
        }
        let result = serve_pairing(&connection, &context).await;
        connection.close(close::DONE.into(), b"pairing done");
        result?;
    } else {
        connection.close(close::NOT_PAIRED.into(), b"unsupported protocol");
    }
    Ok(())
}

/// Close the connection of a key that is not an active device. A revoked
/// device learns that it was revoked (it knows its own key); any other key is
/// simply not paired.
fn refuse(connection: &iroh::endpoint::Connection, context: &ListenerContext, device_key: &str) {
    let known = context.trust.get(device_key);
    let revoked = known
        .as_ref()
        .is_some_and(|device| device.state == DeviceState::Revoked);
    if revoked {
        connection.close(close::REVOKED.into(), b"device revoked");
    } else {
        connection.close(close::NOT_PAIRED.into(), b"device not paired");
    }
    if context.refusals.admit() {
        let event = known.as_ref().map_or_else(
            || AuditEvent::new(AuditKind::Refused).key(device_key),
            |device| AuditEvent::new(AuditKind::Refused).device(device),
        );
        audit(
            &context.trust,
            event.detail(if revoked {
                "revoked device"
            } else if known.is_some() {
                "device not active"
            } else {
                "unknown device"
            }),
        );
    }
}

async fn serve_session(
    connection: iroh::endpoint::Connection,
    context: &ListenerContext,
    live: &Arc<LiveRegistry>,
    streams: &Arc<tokio::sync::Semaphore>,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let device_key = connection.remote_id().to_string();
    // The only check before bmux bytes: the TLS-proven key is an active device.
    let Some(principal) = context.trust.active_principal(&device_key) else {
        refuse(&connection, context, &device_key);
        return;
    };
    let connection_id = Uuid::new_v4();
    let (binding, revocation) = StreamAdmissionBinding::new(principal, connection_id);
    let device = context.trust.get(&device_key);
    let device_name = device
        .as_ref()
        .map(|device| device.name.clone())
        .unwrap_or_default();
    let since = Instant::now();
    let audited = |kind: AuditKind| {
        device.as_ref().map_or_else(
            || AuditEvent::new(kind).key(&device_key),
            |device| AuditEvent::new(kind).device(device),
        )
    };
    let (path, rtt) = path_of(&connection);
    audit(
        &context.trust,
        audited(AuditKind::Connected).detail(format!(
            "via {path}{}",
            rtt.map_or_else(String::new, |rtt| format!(", rtt {}ms", rtt.as_millis()))
        )),
    );
    live.lock().insert(
        connection_id,
        LiveConnection {
            device_key: device_key.clone(),
            device_name,
            connection: connection.clone(),
            revocation: revocation.clone(),
            since,
        },
    );
    // A revocation between the check and registration must not be missed.
    if context.trust.active_principal(&device_key) != Some(principal) {
        live.revoke_device(&device_key, close::REVOKED, b"device revoked");
    }
    {
        let trust = Arc::clone(&context.trust);
        let key = device_key.clone();
        drop(bmux_plugin_sdk::rt::spawn_blocking_named(
            "remote-device-touch",
            move || trust.touch(&key),
        ));
    }
    loop {
        tokio::select! {
            _ = stopped.changed() => break,
            () = binding.revoked() => break,
            stream = connection.accept_bi() => {
                let Ok((mut send, mut recv)) = stream else { break };
                let Ok(permit) = Arc::clone(streams).try_acquire_owned() else {
                    // Too many remote streams: refuse this one, keep the connection.
                    let _ = send.reset(close::BUSY.into());
                    let _ = recv.stop(close::BUSY.into());
                    continue;
                };
                let stream = bmux_ipc::transport::ErasedIpcStream::new(Box::new(CountedStream {
                    inner: tokio::io::join(recv, send),
                    _permit: permit,
                }));
                match context.ingress.accept(stream, Arc::clone(&binding)) {
                    // A full server refuses this stream (it is dropped); others go on.
                    Ok(()) | Err(StreamIngressError::AtCapacity) => {}
                    Err(StreamIngressError::Revoked | StreamIngressError::Unavailable) => break,
                }
            }
        }
    }
    live.lock().remove(&connection_id);
    revocation.revoke();
    let device_revoked =
        binding.is_revoked() && context.trust.active_principal(&device_key).is_none();
    let reason = if device_revoked {
        "device revoked"
    } else if *stopped.borrow() {
        "remote access stopped"
    } else {
        "connection closed"
    };
    if connection.close_reason().is_none() {
        let code = if device_revoked {
            close::REVOKED
        } else {
            close::STOPPING
        };
        connection.close(code.into(), b"closed");
    }
    audit(
        &context.trust,
        audited(AuditKind::Disconnected)
            .detail(format!("{reason} after {}s", since.elapsed().as_secs())),
    );
}

async fn serve_pairing(
    connection: &iroh::endpoint::Connection,
    context: &ListenerContext,
) -> Result<()> {
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connection.accept_bi())
        .await
        .context("pairing stream timed out")??;
    let hello: DeviceMessage =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_frame(&mut recv))
            .await
            .context("pairing hello timed out")??;
    let submitted = match context
        .sessions
        .submit(&context.host_key, connection.remote_id(), &hello)
    {
        Ok(submitted) => submitted,
        Err(reason) => {
            protocol::write_frame(&mut send, &HostMessage::Refused { reason }).await?;
            let _ = send.finish();
            return Ok(());
        }
    };
    protocol::write_frame(
        &mut send,
        &HostMessage::Pending {
            phrase: submitted.phrase.clone(),
        },
    )
    .await?;
    let deadline = tokio::time::Instant::from_std(submitted.expires_at);
    let decision = tokio::select! {
        decision = submitted.decision => decision.unwrap_or(Decision::Refused(Refusal::Expired)),
        () = tokio::time::sleep_until(deadline) => {
            context.sessions.withdraw(&submitted.request_id);
            Decision::Refused(Refusal::Expired)
        }
        _ = connection.closed() => {
            context.sessions.withdraw(&submitted.request_id);
            return Ok(());
        }
    };
    let message = match decision {
        Decision::Approved {
            principal,
            host_name,
            role,
        } => HostMessage::Approved {
            principal,
            host_name,
            role,
        },
        Decision::Refused(reason) => HostMessage::Refused { reason },
    };
    protocol::write_frame(&mut send, &message).await?;
    let _ = send.finish();
    // Let the device read the outcome before the connection closes.
    let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshakes_are_bounded_in_total_and_per_source() {
        let handshakes = Arc::new(Handshakes::default());
        let source = |n: u8| Source::Ip(std::net::IpAddr::from([10, 0, 0, n]));
        let mut held: Vec<_> = (0..MAX_HANDSHAKES_PER_SOURCE)
            .map(|_| handshakes.admit(source(1)).unwrap())
            .collect();
        assert!(handshakes.admit(source(1)).is_none());
        let mut n = 2;
        while held.len() < MAX_HANDSHAKES {
            held.push(handshakes.admit(source(n)).unwrap());
            n += 1;
        }
        assert!(handshakes.admit(source(200)).is_none());
        // A refused source leaves no entry behind.
        assert!(!handshakes.lock().contains_key(&source(200)));
        held.remove(0);
        assert!(handshakes.admit(source(1)).is_some());
        drop(held);
        assert!(handshakes.lock().is_empty());
    }
}
