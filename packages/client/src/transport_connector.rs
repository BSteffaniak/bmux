//! Domain-neutral transport connectors.
//!
//! A transport connector turns an opaque target string (for example a saved
//! host alias owned by a plugin) into a fully handshaken [`BmuxClient`]. Core
//! never parses connector targets: it only asks each registered connector
//! whether it owns a target and lets the single owner connect.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};

use crate::{AttachEndpointConnectFuture, AttachEndpointConnector, BmuxClient, ClientError};

/// Future returned by [`TransportConnector::connect`].
pub type TransportConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BmuxClient, ClientError>> + Send + 'a>>;

/// A plugin-provided way to reach a bmux server for an opaque target.
pub trait TransportConnector: fmt::Debug + Send + Sync + 'static {
    /// Whether this connector owns `target` (for example a saved host alias).
    fn supports(&self, target: &str) -> bool;

    /// Open one fully handshaken client (Hello done) for `target`.
    fn connect<'a>(
        &'a self,
        target: &'a str,
        client_name: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<BmuxClient, ClientError>> + Send + 'a>>;
}

/// Where a connector-backed client came from, so the same connector can open
/// replacement or additional clients for the same target.
#[derive(Clone)]
pub struct TransportOrigin {
    connector: Arc<dyn TransportConnector>,
    target: Arc<str>,
}

impl fmt::Debug for TransportOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransportOrigin")
            .field("connector", &self.connector)
            .field("target", &self.target)
            .finish()
    }
}

impl TransportOrigin {
    /// The target the connector resolved.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// Open another client for the same target through the same connector.
    ///
    /// # Errors
    ///
    /// Returns the connector's connection or handshake failure.
    pub async fn connect(&self, client_name: &'static str) -> Result<BmuxClient, ClientError> {
        let mut client = self.connector.connect(&self.target, client_name).await?;
        client.transport_origin = Some(self.clone());
        Ok(client)
    }
}

/// Failure to resolve a target to one connector.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportConnectorResolutionError {
    /// No registered connector accepted the target.
    #[error("no transport connector accepts target '{target}'")]
    NoConnector { target: String },
    /// More than one registered connector accepted the target.
    #[error("target '{target}' is claimed by more than one transport connector: {connectors:?}")]
    Ambiguous {
        target: String,
        connectors: Vec<String>,
    },
}

/// Failure to connect through the registry.
#[derive(Debug, thiserror::Error)]
pub enum TransportConnectError {
    /// No single connector owns the target.
    #[error(transparent)]
    Resolution(#[from] TransportConnectorResolutionError),
    /// The owning connector failed to open a client.
    #[error("connecting to '{target}' failed: {source}")]
    Connect {
        target: String,
        #[source]
        source: ClientError,
    },
}

#[derive(Debug, Default)]
struct RegistryState {
    connectors: BTreeMap<u64, Arc<dyn TransportConnector>>,
}

/// Thread-safe transport-connector registry. Registering and resolving
/// connectors touches no socket, server or filesystem state.
#[derive(Debug, Default)]
pub struct TransportConnectorRegistry {
    state: RwLock<RegistryState>,
    next_id: AtomicU64,
}

impl TransportConnectorRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a connector until the returned guard is dropped.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub fn register(
        self: &Arc<Self>,
        connector: Arc<dyn TransportConnector>,
    ) -> TransportConnectorRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.state
            .write()
            .expect("transport connector registry poisoned")
            .connectors
            .insert(id, connector);
        TransportConnectorRegistration {
            registry: Arc::downgrade(self),
            id: Some(id),
        }
    }

    /// Whether no connector is registered.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state
            .read()
            .expect("transport connector registry poisoned")
            .connectors
            .is_empty()
    }

    /// Resolve `target` to exactly one connector that supports it.
    ///
    /// # Errors
    ///
    /// Returns no-connector or ambiguity errors.
    ///
    /// # Panics
    ///
    /// Panics if the registry lock is poisoned.
    pub fn resolve(
        &self,
        target: &str,
    ) -> Result<Arc<dyn TransportConnector>, TransportConnectorResolutionError> {
        let mut matches = self
            .state
            .read()
            .expect("transport connector registry poisoned")
            .connectors
            .values()
            .filter(|connector| connector.supports(target))
            .cloned()
            .collect::<Vec<_>>();
        match matches.len() {
            0 => Err(TransportConnectorResolutionError::NoConnector {
                target: target.to_string(),
            }),
            1 => Ok(matches.remove(0)),
            _ => Err(TransportConnectorResolutionError::Ambiguous {
                target: target.to_string(),
                connectors: matches
                    .iter()
                    .map(|connector| format!("{connector:?}"))
                    .collect(),
            }),
        }
    }

    /// Whether exactly one connector owns `target`.
    #[must_use]
    pub fn supports(&self, target: &str) -> bool {
        self.resolve(target).is_ok()
    }

    /// Connect to `target` through its owning connector. The returned client
    /// carries a [`TransportOrigin`] so callers can open replacement clients.
    ///
    /// # Errors
    ///
    /// Returns resolution failures or the connector's connection failure.
    pub async fn connect(
        &self,
        target: &str,
        client_name: &'static str,
    ) -> Result<BmuxClient, TransportConnectError> {
        let connector = self.resolve(target)?;
        let origin = TransportOrigin {
            connector,
            target: Arc::from(target),
        };
        origin
            .connect(client_name)
            .await
            .map_err(|source| TransportConnectError::Connect {
                target: target.to_string(),
                source,
            })
    }
}

impl AttachEndpointConnector for TransportConnectorRegistry {
    fn connect<'a>(
        &'a self,
        target: &'a str,
        client_name: &'static str,
    ) -> AttachEndpointConnectFuture<'a> {
        Box::pin(async move {
            Self::connect(self, target, client_name)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Registration lifetime guard.
#[derive(Debug)]
pub struct TransportConnectorRegistration {
    registry: Weak<TransportConnectorRegistry>,
    id: Option<u64>,
}

impl TransportConnectorRegistration {
    /// Permanently keep this registration for the process lifetime.
    pub fn forget(mut self) {
        let _ = self.id.take();
    }
}

impl Drop for TransportConnectorRegistration {
    fn drop(&mut self) {
        let (Some(registry), Some(id)) = (self.registry.upgrade(), self.id.take()) else {
            return;
        };
        if let Ok(mut state) = registry.state.write() {
            state.connectors.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug)]
    struct PrefixConnector {
        prefix: &'static str,
        connects: Arc<AtomicUsize>,
    }

    impl TransportConnector for PrefixConnector {
        fn supports(&self, target: &str) -> bool {
            target.starts_with(self.prefix)
        }

        fn connect<'a>(
            &'a self,
            _target: &'a str,
            _client_name: &'static str,
        ) -> TransportConnectFuture<'a> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(ClientError::UnexpectedResponse("test connector")) })
        }
    }

    fn prefix(prefix: &'static str) -> (Arc<dyn TransportConnector>, Arc<AtomicUsize>) {
        let connects = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(PrefixConnector {
                prefix,
                connects: Arc::clone(&connects),
            }),
            connects,
        )
    }

    #[test]
    fn resolves_the_single_supporting_connector() {
        let registry = Arc::new(TransportConnectorRegistry::new());
        let (desk, _) = prefix("desk");
        let (lap, _) = prefix("lap");
        let _desk = registry.register(desk);
        let _lap = registry.register(lap);
        let resolved = registry.resolve("desktop").expect("desk connector");
        assert!(resolved.supports("desktop"));
        assert!(!resolved.supports("laptop"));
        assert!(registry.supports("laptop"));
    }

    #[test]
    fn unsupported_target_is_a_structured_error() {
        let registry = Arc::new(TransportConnectorRegistry::new());
        assert!(registry.is_empty());
        let (desk, _) = prefix("desk");
        let _desk = registry.register(desk);
        assert_eq!(
            registry.resolve("server").unwrap_err(),
            TransportConnectorResolutionError::NoConnector {
                target: "server".into()
            }
        );
    }

    #[test]
    fn overlapping_connectors_are_ambiguous() {
        let registry = Arc::new(TransportConnectorRegistry::new());
        let (one, _) = prefix("desk");
        let (two, _) = prefix("de");
        let _one = registry.register(one);
        let _two = registry.register(two);
        assert!(matches!(
            registry.resolve("desktop"),
            Err(TransportConnectorResolutionError::Ambiguous { connectors, .. })
                if connectors.len() == 2
        ));
    }

    #[test]
    fn dropping_the_registration_unregisters() {
        let registry = Arc::new(TransportConnectorRegistry::new());
        let (desk, _) = prefix("desk");
        let registration = registry.register(desk);
        assert!(registry.supports("desktop"));
        drop(registration);
        assert!(registry.is_empty());

        let (kept, _) = prefix("desk");
        registry.register(kept).forget();
        assert!(registry.supports("desktop"));
    }

    #[test]
    fn connect_routes_to_the_owner_only_and_reports_its_failure() {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(connect_routes_to_the_owner_only());
    }

    async fn connect_routes_to_the_owner_only() {
        let registry = Arc::new(TransportConnectorRegistry::new());
        let (desk, desk_connects) = prefix("desk");
        let (lap, lap_connects) = prefix("lap");
        let _desk = registry.register(desk);
        let _lap = registry.register(lap);
        let error = registry.connect("desktop", "test").await.unwrap_err();
        assert!(
            matches!(error, TransportConnectError::Connect { ref target, .. } if target == "desktop")
        );
        assert_eq!(desk_connects.load(Ordering::SeqCst), 1);
        assert_eq!(lap_connects.load(Ordering::SeqCst), 0);

        let missing = registry.connect("server", "test").await.unwrap_err();
        assert!(matches!(
            missing,
            TransportConnectError::Resolution(
                TransportConnectorResolutionError::NoConnector { .. }
            )
        ));
        let endpoint_error = AttachEndpointConnector::connect(registry.as_ref(), "server", "test")
            .await
            .unwrap_err();
        assert!(endpoint_error.contains("no transport connector accepts target 'server'"));
    }
}
