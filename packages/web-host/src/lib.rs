//! Domain-neutral, bounded browser hosting with explicit authenticated remote binding.
//! Encoded snapshots are opaque: callers own presentation semantics and serialization.
#![cfg(unix)]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

#[cfg(test)]
mod acceptance;
mod assets;
mod http;
mod publication;
pub use publication::{BinaryPresentation, Resource};
mod viewport;
pub use viewport::{ViewControl, ViewportMode, ViewportRequest, ViewportRequests};

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

pub use assets::{Asset, ContentType};

/// Resource budgets. Hard ceilings apply even when caller configuration is mistaken.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_snapshot_bytes: usize,
    pub max_asset_bytes: usize,
    pub max_total_asset_bytes: usize,
    pub max_connections: usize,
    pub max_header_bytes: usize,
    pub header_timeout: Duration,
    pub write_timeout: Duration,
    pub poll_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_snapshot_bytes: 16 * 1024 * 1024,
            max_asset_bytes: 8 * 1024 * 1024,
            max_total_asset_bytes: 24 * 1024 * 1024,
            max_connections: 16,
            max_header_bytes: 8192,
            header_timeout: Duration::from_secs(3),
            write_timeout: Duration::from_secs(5),
            poll_timeout: Duration::from_secs(20),
        }
    }
}

impl Limits {
    fn validate(&self) -> io::Result<()> {
        if self.max_snapshot_bytes == 0
            || self.max_snapshot_bytes > 64 * 1024 * 1024
            || self.max_asset_bytes == 0
            || self.max_asset_bytes > 32 * 1024 * 1024
            || self.max_total_asset_bytes == 0
            || self.max_total_asset_bytes > 64 * 1024 * 1024
            || self.max_connections == 0
            || self.max_connections > 64
            || !(256..=16384).contains(&self.max_header_bytes)
            || [self.header_timeout, self.write_timeout, self.poll_timeout]
                .iter()
                .any(|value| value.is_zero() || *value > Duration::from_secs(60))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid hosting limits",
            ));
        }
        Ok(())
    }
}

/// Loopback HTTP authentication policy. Origin and Host checks apply in both modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthMode {
    /// Unauthenticated loopback-only serving.
    #[default]
    None,
    /// Require an ephemeral bearer capability on non-asset routes.
    Token,
}

/// Assets are explicit build output, never discovered recursively or built by this crate.
#[derive(Clone, Debug)]
pub struct HostConfig {
    /// Bind IP; non-loopback requires token authentication.
    pub host: IpAddr,
    /// Browser-facing IP for exact Host/Origin checks. Required for wildcard binds.
    pub public_host: Option<IpAddr>,
    /// HTTP port; zero requests an ephemeral port. Default is 8080.
    pub port: u16,
    pub auth_mode: AuthMode,
    pub asset_directory: PathBuf,
    pub assets: Vec<Asset>,
    pub limits: Limits,
    pub viewport_mode: ViewportMode,
}

impl HostConfig {
    #[must_use]
    pub fn new(asset_directory: PathBuf) -> Self {
        Self {
            host: Ipv4Addr::LOCALHOST.into(),
            public_host: None,
            port: 8080,
            auth_mode: AuthMode::None,
            asset_directory,
            assets: Asset::browser_defaults(),
            limits: Limits::default(),
            viewport_mode: ViewportMode::Observer,
        }
    }
}

/// Immutable opaque full snapshot. Revision is transport-local, not a domain revision.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub revision: u64,
    pub bytes: Arc<[u8]>,
    pub(crate) json_available: bool,
    pub(crate) binary: Option<BinaryPresentation>,
    pub(crate) resources: std::collections::BTreeMap<String, Resource>,
    pub(crate) previous_resources: std::collections::BTreeMap<String, Resource>,
}

/// Cloneable producer; serialized publication preserves strictly increasing revisions.
#[derive(Clone)]
pub struct SnapshotPublisher {
    sender: Arc<Mutex<watch::Sender<Option<Arc<Snapshot>>>>>,
    max_bytes: usize,
}

/// Latest-only delivery: replacement never queues intermediate snapshots.
#[derive(Clone)]
pub struct SnapshotFeed {
    receiver: watch::Receiver<Option<Arc<Snapshot>>>,
    max_bytes: usize,
}

/// Creates a bounded latest-only publication channel.
///
/// # Errors
/// Rejects a zero budget or a budget above the hard 64 MiB ceiling.
pub fn snapshot_feed(max_bytes: usize) -> io::Result<(SnapshotPublisher, SnapshotFeed)> {
    if max_bytes == 0 || max_bytes > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid snapshot limit",
        ));
    }
    let (sender, receiver) = watch::channel(None);
    Ok((
        SnapshotPublisher {
            sender: Arc::new(Mutex::new(sender)),
            max_bytes,
        },
        SnapshotFeed {
            receiver,
            max_bytes,
        },
    ))
}

impl SnapshotPublisher {
    /// The bridge must advance this sequence across encoded frame epoch resets.
    /// Empty, oversized, closed-feed and non-increasing publications are rejected.
    ///
    /// # Errors
    /// Returns an error for an invalid publication or a poisoned publication lock.
    pub fn publish(&self, revision: u64, bytes: Arc<[u8]>) -> io::Result<()> {
        if bytes.is_empty() || bytes.len() > self.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshot exceeds bounds",
            ));
        }
        self.publish_inner(
            revision,
            Some(bytes),
            None,
            std::collections::BTreeMap::new(),
        )
    }

    fn publish_inner(
        &self,
        revision: u64,
        bytes: Option<Arc<[u8]>>,
        binary: Option<BinaryPresentation>,
        resources: std::collections::BTreeMap<String, Resource>,
    ) -> io::Result<()> {
        if bytes
            .as_ref()
            .is_some_and(|bytes| bytes.is_empty() || bytes.len() > self.max_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshot exceeds bounds",
            ));
        }
        let json_available = bytes.is_some();
        let bytes = bytes.unwrap_or_else(|| Arc::from([]));
        let sender = self
            .sender
            .lock()
            .map_err(|_| io::Error::other("publication lock poisoned"))?;
        if sender
            .borrow()
            .as_ref()
            .is_some_and(|current| revision <= current.revision)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "snapshot revision did not advance",
            ));
        }
        let previous_resources = {
            let current = sender.borrow();
            publication::validate(
                self.max_bytes,
                revision,
                binary.as_ref(),
                &resources,
                current.as_deref(),
            )?;
            current
                .as_ref()
                .map_or_else(std::collections::BTreeMap::new, |value| {
                    value.resources.clone()
                })
        };
        sender
            .send(Some(Arc::new(Snapshot {
                revision,
                bytes,
                json_available,
                binary,
                resources,
                previous_resources,
            })))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "snapshot feed closed"))
    }
}

/// Owns all serving tasks. No Debug implementation: the capability is intentionally secret.
pub struct HostHandle {
    address: SocketAddr,
    public_address: SocketAddr,
    token: Option<String>,
    task: Option<JoinHandle<()>>,
    stop: watch::Sender<bool>,
    viewport_requests: Option<ViewportRequests>,
    viewport: Arc<viewport::Viewport>,
}

impl HostHandle {
    /// Restricted cloneable output-generation control; no bearer or lifecycle authority.
    #[must_use]
    pub fn view_control(&self) -> ViewControl {
        ViewControl::new(Arc::clone(&self.viewport))
    }

    /// Updates opaque authoritative generation after successful output publication.
    ///
    /// # Errors
    /// Rejects empty, non-ASCII, or over-128-byte generations.
    pub fn set_generation(&self, generation: &str) -> io::Result<()> {
        self.viewport.set_generation(generation)
    }

    /// Revokes controller ownership and pending geometry on source loss.
    pub fn invalidate_view(&self) {
        self.viewport.invalidate();
    }

    /// Takes the one latest-only viewport receiver; dropping it disables mutation transport.
    pub fn take_viewport_requests(&mut self) -> Option<ViewportRequests> {
        self.viewport_requests.take()
    }

    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Browser-facing URL; in token mode includes a sensitive capability. Never log token URLs.
    #[must_use]
    pub fn bootstrap_url(&self) -> String {
        self.token.as_ref().map_or_else(
            || format!("http://{}/", self.public_address),
            |token| format!("http://{}/#token={token}", self.public_address),
        )
    }

    /// Stops accepting, cancels all in-flight reads/polls/writes, and waits for cleanup.
    pub async fn shutdown(mut self) {
        self.viewport.close();
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HostHandle {
    fn drop(&mut self) {
        self.viewport.close();
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn validate_network(config: &HostConfig) -> io::Result<()> {
    let public = config.public_host.unwrap_or(config.host);
    if public.is_unspecified() || public.is_multicast() || config.host.is_multicast() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "wildcard binds require a concrete public_host IP; multicast is unsupported",
        ));
    }
    if config.host.is_ipv4() != public.is_ipv4() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host and public_host must use the same IP family",
        ));
    }
    if !config.host.is_loopback() && config.auth_mode != AuthMode::Token {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "non-loopback hosting requires token authentication (--web-auth token)",
        ));
    }
    if !config.host.is_unspecified() && config.host != public {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "public_host must match a concrete bind host",
        ));
    }
    Ok(())
}

/// Requires an active Tokio runtime. Defaults to IPv4 loopback; remote binds require tokens.
///
/// # Errors
/// Rejects invalid budgets/assets, unavailable OS entropy, and bind failures.
pub async fn start(config: HostConfig, feed: SnapshotFeed) -> io::Result<HostHandle> {
    config.limits.validate()?;
    validate_network(&config)?;
    if feed.max_bytes > config.limits.max_snapshot_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "feed exceeds host snapshot budget",
        ));
    }
    let assets = assets::load(&config)?;
    let token = if config.auth_mode == AuthMode::Token {
        let mut entropy = [0_u8; 32];
        getrandom::fill(&mut entropy).map_err(|_| io::Error::other("OS randomness unavailable"))?;
        Some(
            entropy
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        )
    } else {
        None
    };
    let listener = TcpListener::bind((config.host, config.port)).await?;
    let address = listener.local_addr()?;
    let public_address = SocketAddr::new(config.public_host.unwrap_or(config.host), address.port());
    let (viewport, viewport_requests) = viewport::Viewport::new(config.viewport_mode);
    let viewport = Arc::new(viewport);
    let state = Arc::new(http::State {
        viewport: Arc::clone(&viewport),
        assets,
        limits: config.limits,
        auth_mode: config.auth_mode,
        token: token.clone().unwrap_or_default(),
        authority: public_address.to_string(),
        origin: format!("http://{public_address}"),
    });
    let (stop, mut stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut feed = feed;
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _ = stopped.changed() => break,
                result = feed.receiver.changed() => { if result.is_err() { break; } },
                Some(_) = connections.join_next(), if !connections.is_empty() => {},
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { break; };
                    if connections.len() >= state.limits.max_connections {
                        drop(stream);
                        continue;
                    }
                    let state = Arc::clone(&state);
                    let feed = feed.clone();
                    connections.spawn(async move { http::serve(stream, state, feed).await; });
                }
            }
        }
        state.viewport.close();
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    });
    Ok(HostHandle {
        viewport_requests: Some(viewport_requests),
        viewport,
        address,
        public_address,
        token,
        task: Some(task),
        stop,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_is_bounded_monotonic_and_latest_only() {
        let (publisher, feed) = snapshot_feed(3).unwrap();
        assert!(publisher.publish(0, Arc::from([])).is_err());
        assert!(publisher.publish(0, Arc::from(*b"long")).is_err());
        publisher.publish(0, Arc::from(*b"one")).unwrap();
        assert!(publisher.publish(0, Arc::from(*b"two")).is_err());
        publisher.publish(9, Arc::from(*b"two")).unwrap();
        assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 9);
        assert_eq!(&*feed.receiver.borrow().as_ref().unwrap().bytes, b"two");
        drop(feed);
        assert!(publisher.publish(10, Arc::from(*b"end")).is_err());
    }

    #[test]
    fn bad_limits_are_rejected() {
        let limits = Limits {
            max_connections: 65,
            ..Limits::default()
        };
        assert!(limits.validate().is_err());
        assert!(snapshot_feed(usize::MAX).is_err());
    }
}
