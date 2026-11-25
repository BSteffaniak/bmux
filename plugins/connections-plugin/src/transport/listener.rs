//! The host listener. `bmux/session/1` connections from active devices hand
//! each bidirectional stream straight to the server (B1 `StreamIngressHandle`)
//! with one revocable binding per connection; anything else is closed right
//! after the handshake, before any bmux bytes are read. `bmux/pair/1` is
//! served only while a pairing session is open.
use super::{PAIR_ALPN, SESSION_ALPN, close, erased, path_of};
use crate::pairing::protocol::{self, DeviceMessage, HostMessage, Refusal};
use crate::pairing::{Decision, Sessions};
use crate::trust::{TrustChange, TrustStore};
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
const MAX_CONNECTIONS: usize = 64;
const MAX_PAIRING_CONNECTIONS: usize = 8;

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

async fn run(
    endpoint: iroh::Endpoint,
    context: ListenerContext,
    live: Arc<LiveRegistry>,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let mut changes = context.trust.subscribe();
    let pairing_slots = Arc::new(tokio::sync::Semaphore::new(MAX_PAIRING_CONNECTIONS));
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
            incoming = endpoint.accept(), if connections.len() < MAX_CONNECTIONS => {
                let Some(incoming) = incoming else { break };
                let context = context.clone();
                let live = Arc::clone(&live);
                let pairing_slots = Arc::clone(&pairing_slots);
                let stopped = stopped.clone();
                connections.spawn(async move {
                    let _ = accept(incoming, context, live, pairing_slots, stopped).await;
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.shutdown().await;
}

async fn accept(
    incoming: iroh::endpoint::Incoming,
    context: ListenerContext,
    live: Arc<LiveRegistry>,
    pairing_slots: Arc<tokio::sync::Semaphore>,
    stopped: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming)
        .await
        .context("handshake timed out")??;
    let alpn = connection.alpn().to_vec();
    if alpn == SESSION_ALPN {
        serve_session(connection, &context, &live, stopped).await;
    } else if alpn == PAIR_ALPN {
        let Ok(_slot) = pairing_slots.try_acquire() else {
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

async fn serve_session(
    connection: iroh::endpoint::Connection,
    context: &ListenerContext,
    live: &Arc<LiveRegistry>,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    let device_key = connection.remote_id().to_string();
    // The only check before bmux bytes: the TLS-proven key is an active device.
    let Some(principal) = context.trust.active_principal(&device_key) else {
        connection.close(close::NOT_PAIRED.into(), b"device not paired");
        return;
    };
    let connection_id = Uuid::new_v4();
    let (binding, revocation) = StreamAdmissionBinding::new(principal, connection_id);
    let device_name = context
        .trust
        .get(&device_key)
        .map(|device| device.name)
        .unwrap_or_default();
    live.lock().insert(
        connection_id,
        LiveConnection {
            device_key: device_key.clone(),
            device_name,
            connection: connection.clone(),
            revocation: revocation.clone(),
            since: Instant::now(),
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
                let Ok((send, recv)) = stream else { break };
                match context.ingress.accept(erased(send, recv), Arc::clone(&binding)) {
                    // A full server refuses this stream (it is dropped); others go on.
                    Ok(()) | Err(StreamIngressError::AtCapacity) => {}
                    Err(StreamIngressError::Revoked | StreamIngressError::Unavailable) => break,
                }
            }
        }
    }
    live.lock().remove(&connection_id);
    revocation.revoke();
    if connection.close_reason().is_none() {
        let code = if binding.is_revoked() && context.trust.active_principal(&device_key).is_none()
        {
            close::REVOKED
        } else {
            close::STOPPING
        };
        connection.close(code.into(), b"closed");
    }
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
        } => HostMessage::Approved {
            principal,
            host_name,
        },
        Decision::Refused(reason) => HostMessage::Refused { reason },
    };
    protocol::write_frame(&mut send, &message).await?;
    let _ = send.finish();
    // Let the device read the outcome before the connection closes.
    let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
    Ok(())
}
