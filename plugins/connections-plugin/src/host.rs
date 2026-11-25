//! Hosting on this machine: the host key, the persisted enable preference, the
//! trust store and listener while hosting, and pairing sessions.
use crate::audit::{Event as AuditEvent, Kind as AuditKind};
use crate::config::{RelayPolicy, RemoteSettings};
use crate::identity::{HostKey, HostLock};
use crate::link::PairingLink;
use crate::pairing::protocol::Refusal;
use crate::pairing::{Decision, Event, Sessions};
use crate::roles::RoleGrants;
use crate::storage::{self, RecordStore};
use crate::transport::listener::{Listener, ListenerContext, LiveSummary};
use crate::trust::{Device, Role, TrustStore};
use anyhow::{Context, Result, ensure};
use bmux_plugin::stream_admission::StreamIngressHandle;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Hosting {
    _lock: HostLock,
    trust: Arc<TrustStore>,
    listener: Listener,
}

/// Non-secret hosting status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub enabled: bool,
    pub source: &'static str,
    pub endpoint_id: String,
    pub bound_addrs: Vec<String>,
    pub relay_mode: &'static str,
    pub relay: Option<String>,
    pub relay_connected: bool,
    pub relay_latency: Option<Duration>,
    pub warning: Option<String>,
}

/// Holds the secret-bearing link: no `Debug`.
#[derive(Clone)]
pub struct OpenedPairing {
    pub session_id: String,
    pub link: String,
    pub expires_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preference {
    version: u32,
    enabled: Option<bool>,
}

fn preference_store(state_dir: &Path) -> Result<RecordStore> {
    let root = storage::connections_dir(state_dir)?;
    RecordStore::new(storage::PrivateDirectory::open(&root)?, "hosting.json")
}

/// The persisted `remote enable` / `remote disable` choice, if any.
pub fn preference(state_dir: &Path) -> Result<Option<bool>> {
    let Some(bytes) = preference_store(state_dir)?.read()? else {
        return Ok(None);
    };
    let preference: Preference = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid hosting preference"))?;
    Ok(preference.enabled)
}

fn set_preference(state_dir: &Path, enabled: bool) -> Result<()> {
    preference_store(state_dir)?.transaction(|bytes| {
        *bytes = Some(serde_json::to_vec(&Preference {
            version: 1,
            enabled: Some(enabled),
        })?);
        Ok(())
    })
}

/// Run `future` on the host runtime (plug-rt) and wait for its output.
///
/// Only for the plugin's synchronous host entries (activation and service
/// bodies), which the host runs on its blocking callback workers so their
/// host calls stay inside the callback entry. Never call it from async code:
/// a runtime worker must not block.
pub fn wait<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    // Detached: the result comes back on the channel. A task that cannot be
    // spawned drops `future` and the sender with it.
    drop(bmux_plugin_sdk::rt::spawn_named(
        "connections-host-entry",
        async move {
            let _ = sender.send(future.await);
        },
    ));
    receiver
        .recv()
        .map_err(|_| anyhow::anyhow!("the host runtime did not run remote access work"))
}

/// This machine's printable host name, for the default saved-host alias.
pub fn host_name() -> String {
    #[cfg(unix)]
    let raw = {
        let mut buffer = [0_u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname NUL-terminates
        // within it on success.
        let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
        let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
        if ok {
            String::from_utf8_lossy(&buffer[..end]).into_owned()
        } else {
            String::new()
        }
    };
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").unwrap_or_default();
    let name: String = raw
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase();
    let name = name.trim_start_matches('-').to_owned();
    if name.is_empty() {
        "host".to_owned()
    } else {
        name
    }
}

pub struct Host {
    state_dir: PathBuf,
    settings: RemoteSettings,
    key: HostKey,
    sessions: Arc<Sessions>,
    hosting: Mutex<Option<Hosting>>,
    warning: Mutex<Option<String>>,
    ingress: Mutex<Option<StreamIngressHandle>>,
}

impl Host {
    pub fn new(state_dir: &Path, settings: RemoteSettings) -> Result<Self> {
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            key: HostKey::load_or_create(state_dir)?,
            settings,
            sessions: Arc::default(),
            hosting: Mutex::new(None),
            warning: Mutex::new(None),
            ingress: Mutex::new(None),
        })
    }

    pub fn set_ingress(&self, ingress: Option<StreamIngressHandle>) {
        *lock(&self.ingress) = ingress;
    }

    fn source(&self, preference: Option<bool>) -> &'static str {
        source(&self.settings, preference)
    }

    pub fn status(&self) -> Status {
        let hosting = lock(&self.hosting);
        let (bound_addrs, relay) = hosting.as_ref().map_or_else(
            || (Vec::new(), RelayState::default()),
            |hosting| {
                let endpoint = hosting.listener.endpoint();
                let mut bound: Vec<_> = endpoint
                    .bound_sockets()
                    .into_iter()
                    .map(|addr| addr.to_string())
                    .collect();
                bound.sort();
                (bound, relay_state(endpoint))
            },
        );
        Status {
            enabled: hosting.is_some(),
            source: self.source(preference(&self.state_dir).ok().flatten()),
            endpoint_id: self.key.public().to_string(),
            bound_addrs,
            relay_mode: match self.settings.relay {
                RelayPolicy::Default => "default",
                RelayPolicy::Disabled => "disabled",
                RelayPolicy::Custom { .. } => "custom",
            },
            relay: relay.url,
            relay_connected: relay.connected,
            relay_latency: relay.latency,
            warning: lock(&self.warning).clone(),
        }
    }

    /// Start hosting (idempotent). Fails when configuration prohibits it,
    /// another runtime hosts, or the server offers no stream ingress.
    pub fn start(&self, grants: &dyn RoleGrants) -> Result<()> {
        ensure!(
            self.settings.allow_full_runtime_ingress != Some(false),
            "remote hosting is prohibited by allow_full_runtime_ingress = false in the connections plugin settings"
        );
        let mut hosting = lock(&self.hosting);
        if hosting.is_some() {
            return Ok(());
        }
        let ingress = lock(&self.ingress)
            .clone()
            .context("this server does not accept remote streams")?;
        let host_lock = HostLock::acquire(&self.state_dir)?;
        let trust = Arc::new(TrustStore::open(&storage::connections_dir(
            &self.state_dir,
        )?)?);
        if let Err(error) = trust.recover(|principal| Ok(grants.get(principal)?.is_some())) {
            tracing::warn!("pending device recovery deferred: {error:#}");
        }
        let builder = crate::transport::builder(
            self.key.secret(),
            &self.settings.relay,
            self.settings.bind_addr,
            &self.settings.external_addrs,
        )?;
        let context = ListenerContext {
            trust: Arc::clone(&trust),
            sessions: Arc::clone(&self.sessions),
            ingress,
            host_key: self.key.public(),
            refusals: Arc::default(),
        };
        let listener = wait(Listener::start(builder, context))??;
        *hosting = Some(Hosting {
            _lock: host_lock,
            trust,
            listener,
        });
        drop(hosting);
        *lock(&self.warning) = None;
        Ok(())
    }

    /// Stop hosting from a synchronous host entry: every remote connection
    /// closes.
    pub fn stop(&self) -> Result<()> {
        let hosting = lock(&self.hosting).take();
        if let Some(hosting) = hosting {
            wait(Self::close(hosting))?;
        }
        Ok(())
    }

    /// Stop hosting from async code (deactivation): every remote connection
    /// closes.
    pub async fn shutdown(&self) {
        let hosting = lock(&self.hosting).take();
        if let Some(hosting) = hosting {
            Self::close(hosting).await;
        }
    }

    async fn close(mut hosting: Hosting) {
        hosting.listener.stop().await;
        drop(hosting);
    }

    /// Start at activation when configuration and the persisted choice allow.
    /// A bind failure is a warning: local terminals keep working.
    pub fn start_at_activation(&self, grants: &dyn RoleGrants) -> Result<()> {
        let preference = preference(&self.state_dir)?;
        if !self.settings.startup_enabled(preference) {
            return Ok(());
        }
        if let Err(error) = self.start(grants) {
            let warning = format!(
                "Remote access unavailable: {}. Local terminals remain available. Use `bmux remote enable --yes` to retry.",
                if error
                    .chain()
                    .any(<dyn std::error::Error>::is::<crate::transport::listener::BindFailure>)
                {
                    "could not bind the configured remote endpoint".to_owned()
                } else {
                    format!("{error:#}")
                }
            );
            tracing::warn!("{warning}");
            *lock(&self.warning) = Some(warning);
        }
        Ok(())
    }

    pub fn set_enabled(&self, enabled: bool, grants: &dyn RoleGrants) -> Result<Status> {
        if enabled {
            ensure!(
                self.settings.allow_full_runtime_ingress != Some(false),
                "remote hosting is prohibited by allow_full_runtime_ingress = false; change the connections plugin settings and restart first"
            );
            self.start(grants)?;
            set_preference(&self.state_dir, true)?;
        } else {
            set_preference(&self.state_dir, false)?;
            self.stop()?;
        }
        Ok(self.status())
    }

    /// Run `f` on the trust store: the hosting one, or one opened briefly.
    fn with_trust<T>(
        &self,
        grants: &dyn RoleGrants,
        f: impl FnOnce(&TrustStore) -> Result<T>,
    ) -> Result<T> {
        if let Some(hosting) = lock(&self.hosting).as_ref() {
            return f(&hosting.trust);
        }
        let _lock = HostLock::acquire(&self.state_dir)?;
        let trust = TrustStore::open(&storage::connections_dir(&self.state_dir)?)?;
        trust.recover(|principal| Ok(grants.get(principal)?.is_some()))?;
        f(&trust)
    }

    pub fn list_devices(&self, grants: &dyn RoleGrants) -> Result<Vec<Device>> {
        self.with_trust(grants, |trust| Ok(trust.list()))
    }

    /// The newest `limit` audit events, oldest first.
    pub fn audit_log(&self, limit: usize, grants: &dyn RoleGrants) -> Result<Vec<AuditEvent>> {
        self.with_trust(grants, |trust| trust.audit(limit))
    }

    pub fn live_connections(&self) -> Vec<LiveSummary> {
        lock(&self.hosting)
            .as_ref()
            .map_or_else(Vec::new, |hosting| hosting.listener.live().summaries())
    }

    /// Revoke: the device's connections close now and reconnects are refused.
    pub fn revoke(&self, selector: &str, grants: &dyn RoleGrants) -> Result<Device> {
        self.with_trust(grants, |trust| {
            let device = trust.find(selector)?;
            let revoked = trust.revoke(&device.key)?;
            if let Err(error) = grants.clear(device.principal) {
                tracing::warn!("revoked device keeps a stale role grant: {error:#}");
            }
            trust.record(&AuditEvent::new(AuditKind::Revoked).device(&revoked));
            Ok(revoked)
        })
    }

    pub fn set_role(&self, selector: &str, role: Role, grants: &dyn RoleGrants) -> Result<Device> {
        self.with_trust(grants, |trust| {
            let device = trust.find(selector)?;
            ensure!(
                device.state == crate::trust::DeviceState::Active,
                "device '{}' is not active",
                device.name
            );
            grants.set(device.principal, role)?;
            let changed = trust.set_role(&device.key, role)?;
            trust.record(
                &AuditEvent::new(AuditKind::RoleChanged)
                    .device(&changed)
                    .detail(format!("was {}", role_name(device.role))),
            );
            Ok(changed)
        })
    }

    pub fn begin_pairing(
        &self,
        role: Role,
        name: Option<String>,
        ttl: Duration,
    ) -> Result<OpenedPairing> {
        if let Some(name) = &name {
            crate::known_hosts::valid_alias(name).context("invalid device name")?;
        }
        let endpoint = lock(&self.hosting)
            .as_ref()
            .map(|hosting| hosting.listener.endpoint().clone())
            .context("remote hosting is off; run `bmux remote enable` first")?;
        if !matches!(self.settings.relay, RelayPolicy::Disabled) {
            // Give the relay a moment so the link carries a relay hint.
            let waiting = endpoint.clone();
            wait(async move {
                let _ = tokio::time::timeout(Duration::from_secs(3), waiting.online()).await;
            })?;
        }
        let addr = endpoint.addr();
        let ttl = if ttl.is_zero() {
            crate::pairing::DEFAULT_TTL
        } else {
            ttl
        };
        let opened = self.sessions.open(role, name, ttl)?;
        // Configured external addresses first, then what the endpoint knows.
        let mut addrs = self.settings.external_addrs.clone();
        let mut known: Vec<_> = addr.ip_addrs().copied().collect();
        if known.is_empty() {
            known = endpoint
                .bound_sockets()
                .into_iter()
                .filter(|addr| !addr.ip().is_unspecified())
                .collect();
        }
        for addr in known {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
        let link = PairingLink {
            host_key: self.key.public(),
            secret: opened.secret,
            relay: addr
                .relay_urls()
                .next()
                .map(ToString::to_string)
                .or_else(|| {
                    // Not connected to the relay yet: hint the configured one.
                    match &self.settings.relay {
                        RelayPolicy::Custom { urls } => urls.first().cloned(),
                        RelayPolicy::Default | RelayPolicy::Disabled => None,
                    }
                }),
            addrs,
        }
        .encode()?;
        self.set_pairing(true);
        Ok(OpenedPairing {
            session_id: opened.session_id,
            link,
            expires_at: opened.expires_at_unix,
        })
    }

    fn set_pairing(&self, open: bool) {
        if let Some(hosting) = lock(&self.hosting).as_ref() {
            hosting.listener.set_pairing(open);
        }
    }

    pub fn next_pairing_event(&self, session_id: &str, timeout: Duration) -> Result<Event> {
        let sessions = Arc::clone(&self.sessions);
        let session_id = session_id.to_owned();
        let event = wait(async move { sessions.next_event(&session_id, timeout).await })??;
        if matches!(event, Event::Ended { .. }) {
            self.set_pairing(false);
        }
        Ok(event)
    }

    /// Approve or reject a waiting request. Approval is the recoverable
    /// transition: device pending -> role granted -> device active.
    pub fn decide_pairing(
        &self,
        session_id: &str,
        request_id: &str,
        approve: bool,
        grants: &dyn RoleGrants,
    ) -> Result<Option<Device>> {
        let approval = self.sessions.take(session_id, request_id)?;
        if !approve {
            let mut event = AuditEvent::new(AuditKind::PairRejected)
                .key(&approval.device_key.to_string())
                .detail("rejected by the owner");
            event.device_name = Some(approval.name.clone());
            event.role = Some(approval.role);
            approval.resolve(Decision::Refused(Refusal::Rejected));
            let _ = self.with_trust(grants, |trust| {
                trust.record(&event);
                Ok(())
            });
            return Ok(None);
        }
        let key = approval.device_key.to_string();
        let result = self.with_trust(grants, |trust| {
            let pending = trust.stage_pending(&key, &approval.name, approval.role)?;
            if let Err(error) = grants.set(pending.principal, approval.role) {
                trust.discard_pending(&key)?;
                return Err(error.context("granting the role failed; the device was not paired"));
            }
            let device = trust.activate(&key)?;
            trust.record(&AuditEvent::new(AuditKind::Paired).device(&device));
            Ok(device)
        });
        match result {
            Ok(device) => {
                approval.resolve(Decision::Approved {
                    principal: device.principal,
                    host_name: host_name(),
                });
                // The secret is single-use.
                self.sessions.end(session_id, Refusal::Expired);
                self.set_pairing(false);
                Ok(Some(device))
            }
            Err(error) => {
                approval.resolve(Decision::Refused(Refusal::Failed));
                Err(error)
            }
        }
    }

    pub fn end_pairing(&self, session_id: &str) {
        self.sessions.close(session_id);
        self.set_pairing(false);
    }
}

/// Where the hosting choice comes from, in the order the policy applies.
pub fn source(settings: &RemoteSettings, preference: Option<bool>) -> &'static str {
    if settings.allow_full_runtime_ingress == Some(false) {
        return "config: prohibited";
    }
    match preference {
        Some(true) => "remote enable",
        Some(false) => "remote disable",
        None if settings.startup_enabled(None) => "config: auto_start",
        None if settings.allow_full_runtime_ingress == Some(true) => "config: allowed",
        None => "default",
    }
}

/// A sentence for `source`: why hosting is on or off.
pub fn explain_source(enabled: bool, source: &str) -> &'static str {
    match (enabled, source) {
        (_, "config: prohibited") => {
            "prohibited by allow_full_runtime_ingress = false in the connections plugin settings"
        }
        (true, "remote enable") => "turned on with `bmux remote enable`",
        (false, "remote enable") => {
            "turned on with `bmux remote enable`, but not running here (see the warning, or start bmux)"
        }
        (_, "remote disable") => "turned off with `bmux remote disable`",
        (true, "config: auto_start") => "started by remote.auto_start in the configuration",
        (false, "config: auto_start") => {
            "remote.auto_start is set, but hosting is not running here (see the warning, or start bmux)"
        }
        (_, "config: allowed") => "allowed by configuration; turn it on with `bmux remote enable`",
        _ => "off by default; turn it on with `bmux remote enable`",
    }
}

pub const fn role_name(role: Role) -> &'static str {
    match role {
        Role::Full => "full",
        Role::ViewOnly => "view-only",
    }
}

/// The home relay, whether the endpoint reaches it, and its latency.
#[derive(Clone, Debug, Default)]
pub struct RelayState {
    pub url: Option<String>,
    pub connected: bool,
    pub latency: Option<Duration>,
}

/// The relay state of `endpoint`, from its home relay status and its latest
/// network report.
pub fn relay_state(endpoint: &iroh::Endpoint) -> RelayState {
    use iroh::Watcher as _;
    let home = endpoint.home_relay_status().get();
    let connected = home.iter().find(|relay| relay.is_connected());
    let url = connected
        .or_else(|| home.first())
        .map(|relay| relay.url().clone())
        .or_else(|| endpoint.addr().relay_urls().next().cloned());
    let latency = endpoint.net_report().get().and_then(|report| {
        let wanted = url.as_ref().or(report.preferred_relay.as_ref())?;
        report
            .relay_latency
            .iter()
            .filter(|(_, relay, _)| *relay == wanted)
            .map(|(_, _, latency)| latency)
            .min()
    });
    RelayState {
        url: url.map(|url| url.to_string()),
        connected: connected.is_some(),
        latency,
    }
}

/// What this machine has stored about hosting, read without a server and
/// without creating anything: `None` fields were never written.
pub struct Stored {
    pub host_key: Option<iroh::PublicKey>,
    pub preference: Option<bool>,
}

/// Read the stored host key and hosting choice; creates nothing.
pub fn stored(state_dir: &Path) -> Result<Stored> {
    let directory = state_dir.join("connections");
    if !directory.is_dir() {
        return Ok(Stored {
            host_key: None,
            preference: None,
        });
    }
    let host_key = if directory.join("host.key").is_file() {
        Some(HostKey::load_or_create(state_dir)?.public())
    } else {
        None
    };
    let preference = if directory.join("hosting.json").is_file() {
        preference(state_dir)?
    } else {
        None
    };
    Ok(Stored {
        host_key,
        preference,
    })
}

/// Run `f` on this machine's trust store while no server hosts, under the
/// host lock. `None` when this machine never hosted (nothing is created).
pub fn offline_trust<T>(
    state_dir: &Path,
    f: impl FnOnce(&TrustStore) -> Result<T>,
) -> Result<Option<T>> {
    if !state_dir.join("connections").join("trust.redb").is_file() {
        return Ok(None);
    }
    let _lock = HostLock::acquire(state_dir)?;
    let trust = TrustStore::open(&storage::connections_dir(state_dir)?)?;
    f(&trust).map(Some)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
