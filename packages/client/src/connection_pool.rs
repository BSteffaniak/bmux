//! Domain-neutral endpoint connection pooling and admission backpressure.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio::time::Instant;

/// Hard limits for endpoint connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionPoolLimits {
    /// Maximum live connections across all endpoints, including idle clients.
    pub max_connections: usize,
    /// Maximum live connections for one endpoint, including idle clients.
    pub max_connections_per_endpoint: usize,
    /// Maximum reusable idle connections retained for one endpoint.
    pub max_idle_per_endpoint: usize,
}

impl Default for ConnectionPoolLimits {
    fn default() -> Self {
        Self {
            max_connections: 256,
            max_connections_per_endpoint: 64,
            max_idle_per_endpoint: 4,
        }
    }
}

/// Invalid pool configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionPoolConfigError {
    /// Global connection capacity must be non-zero.
    #[error("global connection capacity must be greater than zero")]
    ZeroGlobalCapacity,
    /// Per-endpoint capacity must be non-zero.
    #[error("per-endpoint connection capacity must be greater than zero")]
    ZeroEndpointCapacity,
    /// Per-endpoint capacity cannot exceed global capacity.
    #[error("per-endpoint connection capacity cannot exceed global capacity")]
    EndpointCapacityExceedsGlobal,
    /// Idle retention cannot exceed per-endpoint capacity.
    #[error("per-endpoint idle retention cannot exceed per-endpoint capacity")]
    IdleCapacityExceedsEndpoint,
}

/// Failure to acquire a pooled endpoint connection.
#[derive(Debug)]
pub enum ConnectionPoolAcquireError<E> {
    /// Admission remained saturated until the caller's deadline.
    AdmissionTimedOut,
    /// Creating a new connection failed.
    Connect(E),
}

impl<E: fmt::Display> fmt::Display for ConnectionPoolAcquireError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AdmissionTimedOut => formatter.write_str("connection pool admission timed out"),
            Self::Connect(error) => write!(formatter, "endpoint connection failed: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for ConnectionPoolAcquireError<E> {}

#[derive(Debug)]
struct EndpointState<T> {
    active: usize,
    total: usize,
    idle: VecDeque<T>,
}

impl<T> Default for EndpointState<T> {
    fn default() -> Self {
        Self {
            active: 0,
            total: 0,
            idle: VecDeque::new(),
        }
    }
}

#[derive(Debug)]
struct PoolState<T> {
    active: usize,
    total: usize,
    endpoints: BTreeMap<String, EndpointState<T>>,
}

impl<T> Default for PoolState<T> {
    fn default() -> Self {
        Self {
            active: 0,
            total: 0,
            endpoints: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct PoolInner<T> {
    limits: ConnectionPoolLimits,
    state: Mutex<PoolState<T>>,
    changed: Notify,
}

/// A pool keyed by opaque endpoint identity.
#[derive(Debug)]
pub struct EndpointConnectionPool<T> {
    inner: Arc<PoolInner<T>>,
}

impl<T> Clone for EndpointConnectionPool<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T> EndpointConnectionPool<T> {
    /// Create a pool with validated hard limits.
    ///
    /// # Errors
    ///
    /// Returns an error for zero or internally inconsistent capacities.
    pub fn new(limits: ConnectionPoolLimits) -> Result<Self, ConnectionPoolConfigError> {
        if limits.max_connections == 0 {
            return Err(ConnectionPoolConfigError::ZeroGlobalCapacity);
        }
        if limits.max_connections_per_endpoint == 0 {
            return Err(ConnectionPoolConfigError::ZeroEndpointCapacity);
        }
        if limits.max_connections_per_endpoint > limits.max_connections {
            return Err(ConnectionPoolConfigError::EndpointCapacityExceedsGlobal);
        }
        if limits.max_idle_per_endpoint > limits.max_connections_per_endpoint {
            return Err(ConnectionPoolConfigError::IdleCapacityExceedsEndpoint);
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                limits,
                state: Mutex::new(PoolState::default()),
                changed: Notify::new(),
            }),
        })
    }

    /// Acquire one connection before `deadline`, reusing an idle client or
    /// invoking `connect` after admission reserves new capacity.
    ///
    /// The connector is called at most once. Its future is bounded by the same
    /// deadline as admission.
    ///
    /// # Panics
    ///
    /// Panics if the internal pool lock is poisoned.
    ///
    /// # Errors
    ///
    /// Returns timeout when admission or connection establishment exceeds the
    /// deadline, or wraps the connector's error.
    pub async fn acquire<F, Fut, E>(
        &self,
        endpoint: impl Into<String>,
        deadline: Instant,
        connect: F,
    ) -> Result<EndpointConnectionLease<T>, ConnectionPoolAcquireError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let endpoint = endpoint.into();
        let mut connect = Some(connect);
        loop {
            let notified = self.inner.changed.notified();
            let admission = {
                let mut state = self
                    .inner
                    .state
                    .lock()
                    .expect("connection pool lock poisoned");
                reserve_admission(&mut state, &self.inner.limits, &endpoint)
            };
            match admission {
                Some(Admission::Idle(client)) => {
                    return Ok(EndpointConnectionLease::new(
                        Arc::clone(&self.inner),
                        endpoint,
                        client,
                    ));
                }
                Some(Admission::Connect) => {
                    let connector = connect
                        .take()
                        .expect("connector is consumed only after admission");
                    let remaining =
                        deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| {
                                release_reserved_connection(&self.inner, &endpoint);
                                ConnectionPoolAcquireError::AdmissionTimedOut
                            })?;
                    match tokio::time::timeout(remaining, connector()).await {
                        Ok(Ok(client)) => {
                            return Ok(EndpointConnectionLease::new(
                                Arc::clone(&self.inner),
                                endpoint,
                                client,
                            ));
                        }
                        Ok(Err(error)) => {
                            release_reserved_connection(&self.inner, &endpoint);
                            return Err(ConnectionPoolAcquireError::Connect(error));
                        }
                        Err(_) => {
                            release_reserved_connection(&self.inner, &endpoint);
                            return Err(ConnectionPoolAcquireError::AdmissionTimedOut);
                        }
                    }
                }
                None => {
                    let remaining = deadline
                        .checked_duration_since(Instant::now())
                        .ok_or(ConnectionPoolAcquireError::AdmissionTimedOut)?;
                    if tokio::time::timeout(remaining, notified).await.is_err() {
                        return Err(ConnectionPoolAcquireError::AdmissionTimedOut);
                    }
                }
            }
        }
    }

    /// Snapshot `(active, total, idle)` counts for one endpoint.
    ///
    /// # Panics
    ///
    /// Panics if the internal pool lock is poisoned.
    #[must_use]
    pub fn endpoint_counts(&self, endpoint: &str) -> (usize, usize, usize) {
        let state = self
            .inner
            .state
            .lock()
            .expect("connection pool lock poisoned");
        state.endpoints.get(endpoint).map_or((0, 0, 0), |entry| {
            (entry.active, entry.total, entry.idle.len())
        })
    }

    /// Snapshot `(active, total, idle)` counts for diagnostics and tests.
    ///
    /// # Panics
    ///
    /// Panics if the internal pool lock is poisoned.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let state = self
            .inner
            .state
            .lock()
            .expect("connection pool lock poisoned");
        let idle = state
            .endpoints
            .values()
            .map(|endpoint| endpoint.idle.len())
            .sum();
        (state.active, state.total, idle)
    }
}

#[derive(Debug)]
enum Admission<T> {
    Idle(T),
    Connect,
}

fn reserve_admission<T>(
    state: &mut PoolState<T>,
    limits: &ConnectionPoolLimits,
    endpoint: &str,
) -> Option<Admission<T>> {
    let endpoint_active = state
        .endpoints
        .get(endpoint)
        .map_or(0, |entry| entry.active);
    if state.active >= limits.max_connections
        || endpoint_active >= limits.max_connections_per_endpoint
    {
        return None;
    }

    if let Some(client) = state
        .endpoints
        .get_mut(endpoint)
        .and_then(|entry| entry.idle.pop_front())
    {
        state.active += 1;
        state
            .endpoints
            .get_mut(endpoint)
            .expect("endpoint exists after idle pop")
            .active += 1;
        return Some(Admission::Idle(client));
    }

    let endpoint_total = state.endpoints.get(endpoint).map_or(0, |entry| entry.total);
    if endpoint_total >= limits.max_connections_per_endpoint {
        return None;
    }

    if state.total >= limits.max_connections && !evict_one_idle(state, endpoint) {
        return None;
    }

    let entry = state.endpoints.entry(endpoint.to_string()).or_default();
    entry.active += 1;
    entry.total += 1;
    state.active += 1;
    state.total += 1;
    Some(Admission::Connect)
}

fn evict_one_idle<T>(state: &mut PoolState<T>, requested_endpoint: &str) -> bool {
    let candidate = state
        .endpoints
        .iter()
        .find(|(endpoint, entry)| endpoint.as_str() != requested_endpoint && !entry.idle.is_empty())
        .map(|(endpoint, _)| endpoint.clone());
    let Some(candidate) = candidate else {
        return false;
    };
    let entry = state
        .endpoints
        .get_mut(&candidate)
        .expect("idle eviction candidate exists");
    let _ = entry.idle.pop_front();
    entry.total -= 1;
    state.total -= 1;
    if entry.total == 0 && entry.active == 0 {
        state.endpoints.remove(&candidate);
    }
    true
}

fn release_reserved_connection<T>(inner: &Arc<PoolInner<T>>, endpoint: &str) {
    let mut state = inner.state.lock().expect("connection pool lock poisoned");
    release_counts(&mut state, endpoint, true);
    drop(state);
    inner.changed.notify_waiters();
}

fn release_counts<T>(state: &mut PoolState<T>, endpoint: &str, discard: bool) {
    state.active -= 1;
    let entry = state
        .endpoints
        .get_mut(endpoint)
        .expect("active endpoint has pool state");
    entry.active -= 1;
    if discard {
        entry.total -= 1;
        state.total -= 1;
    }
    if entry.active == 0 && entry.total == 0 {
        state.endpoints.remove(endpoint);
    }
}

/// Exclusive pooled connection lease.
#[derive(Debug)]
pub struct EndpointConnectionLease<T> {
    inner: Option<Arc<PoolInner<T>>>,
    endpoint: String,
    client: Option<T>,
    reusable: bool,
}

impl<T> EndpointConnectionLease<T> {
    const fn new(inner: Arc<PoolInner<T>>, endpoint: String, client: T) -> Self {
        Self {
            inner: Some(inner),
            endpoint,
            client: Some(client),
            reusable: true,
        }
    }

    /// Prevent this connection from returning to the idle pool.
    pub const fn mark_unhealthy(&mut self) {
        self.reusable = false;
    }

    /// Opaque endpoint identity associated with this lease.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Consume the reusable lease into dedicated long-lived ownership.
    ///
    /// Capacity remains charged until the dedicated lease is dropped, and the
    /// value is never returned to the idle pool. This is the ownership seam for
    /// streams whose protocol state cannot safely be reused as a request client.
    /// # Panics
    ///
    /// Panics only if this lease's internal ownership invariant is violated.
    #[must_use]
    pub fn into_dedicated(mut self) -> DedicatedEndpointConnection<T> {
        let inner = self.inner.take().expect("live lease has pool ownership");
        let value = self.client.take().expect("live lease has a connection");
        DedicatedEndpointConnection {
            inner: Some(inner),
            endpoint: self.endpoint.clone(),
            value: Some(value),
        }
    }
}

impl EndpointConnectionLease<crate::BmuxClient> {
    /// Consume a pooled handshaken client into a dedicated streaming client.
    ///
    /// The connection remains charged against pool limits until the returned
    /// stream lease is dropped and is never returned to the request-client
    /// idle pool.
    ///
    /// # Errors
    ///
    /// Returns an error when streaming frame processing cannot be initialized.
    pub fn into_streaming(
        self,
    ) -> crate::Result<DedicatedEndpointConnection<crate::StreamingBmuxClient, crate::BmuxClient>>
    {
        self.into_dedicated()
            .try_map(crate::StreamingBmuxClient::from_client)
    }
}

impl<T> Deref for EndpointConnectionLease<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.client.as_ref().expect("live lease has a connection")
    }
}

impl<T> DerefMut for EndpointConnectionLease<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.client.as_mut().expect("live lease has a connection")
    }
}

impl<T> Drop for EndpointConnectionLease<T> {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let client = self.client.take().expect("live lease has a connection");
        let mut state = inner.state.lock().expect("connection pool lock poisoned");
        let retain = self.reusable
            && state
                .endpoints
                .get(&self.endpoint)
                .is_some_and(|entry| entry.idle.len() < inner.limits.max_idle_per_endpoint);
        if retain {
            release_counts(&mut state, &self.endpoint, false);
            state
                .endpoints
                .get_mut(&self.endpoint)
                .expect("retained endpoint exists")
                .idle
                .push_back(client);
        } else {
            release_counts(&mut state, &self.endpoint, true);
            drop(client);
        }
        drop(state);
        inner.changed.notify_waiters();
    }
}

/// Dedicated endpoint ownership that keeps pool capacity charged.
#[derive(Debug)]
pub struct DedicatedEndpointConnection<T, P = T> {
    inner: Option<Arc<PoolInner<P>>>,
    endpoint: String,
    value: Option<T>,
}

impl<T, P> DedicatedEndpointConnection<T, P> {
    /// Transform the dedicated value while retaining the same admission slot.
    ///
    /// # Panics
    ///
    /// Panics only if this dedicated lease's internal ownership invariant is
    /// violated.
    ///
    /// # Errors
    ///
    /// On conversion failure the original value is dropped and capacity is
    /// released; the conversion error is returned.
    pub fn try_map<U, E>(
        mut self,
        convert: impl FnOnce(T) -> Result<U, E>,
    ) -> Result<DedicatedEndpointConnection<U, P>, E> {
        let value = self.value.take().expect("live dedicated lease has a value");
        match convert(value) {
            Ok(value) => {
                let inner = self
                    .inner
                    .take()
                    .expect("live dedicated lease has pool ownership");
                let endpoint = self.endpoint.clone();
                Ok(DedicatedEndpointConnection {
                    inner: Some(inner),
                    endpoint,
                    value: Some(value),
                })
            }
            Err(error) => Err(error),
        }
    }
}

impl<T, P> Deref for DedicatedEndpointConnection<T, P> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.value
            .as_ref()
            .expect("live dedicated lease has a value")
    }
}

impl<T, P> DerefMut for DedicatedEndpointConnection<T, P> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
            .as_mut()
            .expect("live dedicated lease has a value")
    }
}

impl<T, P> Drop for DedicatedEndpointConnection<T, P> {
    fn drop(&mut self) {
        let _ = self.value.take();
        let Some(inner) = self.inner.take() else {
            return;
        };
        let mut state = inner.state.lock().expect("connection pool lock poisoned");
        release_counts(&mut state, &self.endpoint, true);
        drop(state);
        inner.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn pool(limits: ConnectionPoolLimits) -> EndpointConnectionPool<usize> {
        EndpointConnectionPool::new(limits).expect("valid limits")
    }

    #[test]
    fn rejects_invalid_limits() {
        assert!(matches!(
            EndpointConnectionPool::<usize>::new(ConnectionPoolLimits {
                max_connections: 0,
                max_connections_per_endpoint: 1,
                max_idle_per_endpoint: 0,
            }),
            Err(ConnectionPoolConfigError::ZeroGlobalCapacity)
        ));
        assert!(matches!(
            EndpointConnectionPool::<usize>::new(ConnectionPoolLimits {
                max_connections: 1,
                max_connections_per_endpoint: 2,
                max_idle_per_endpoint: 0,
            }),
            Err(ConnectionPoolConfigError::EndpointCapacityExceedsGlobal)
        ));
        assert!(matches!(
            EndpointConnectionPool::<usize>::new(ConnectionPoolLimits {
                max_connections: 2,
                max_connections_per_endpoint: 1,
                max_idle_per_endpoint: 2,
            }),
            Err(ConnectionPoolConfigError::IdleCapacityExceedsEndpoint)
        ));
    }

    #[tokio::test]
    async fn successful_lease_is_reused_without_reconnecting() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 2,
            max_connections_per_endpoint: 2,
            max_idle_per_endpoint: 1,
        });
        let connects = AtomicUsize::new(0);
        {
            let lease = pool
                .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                    connects.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(7)
                })
                .await
                .expect("first lease");
            assert_eq!(*lease, 7);
        }
        let lease = pool
            .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                connects.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(8)
            })
            .await
            .expect("reused lease");
        assert_eq!(*lease, 7);
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unhealthy_lease_is_discarded() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 1,
            max_connections_per_endpoint: 1,
            max_idle_per_endpoint: 1,
        });
        let mut lease = pool
            .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(1)
            })
            .await
            .expect("lease");
        lease.mark_unhealthy();
        drop(lease);
        assert_eq!(pool.counts(), (0, 0, 0));
    }

    #[tokio::test]
    async fn saturated_pool_applies_deadline_backpressure() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 1,
            max_connections_per_endpoint: 1,
            max_idle_per_endpoint: 0,
        });
        let held = pool
            .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(1)
            })
            .await
            .expect("held lease");
        let error = pool
            .acquire("a", Instant::now() + Duration::from_millis(10), || async {
                Ok::<_, ()>(2)
            })
            .await
            .expect_err("admission should time out");
        assert!(matches!(
            error,
            ConnectionPoolAcquireError::AdmissionTimedOut
        ));
        drop(held);
    }

    #[tokio::test]
    async fn dropping_lease_wakes_waiter() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 1,
            max_connections_per_endpoint: 1,
            max_idle_per_endpoint: 1,
        });
        let held = pool
            .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(1)
            })
            .await
            .expect("held lease");
        let waiter_pool = pool.clone();
        let waiter = tokio::spawn(async move {
            waiter_pool
                .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                    Ok::<_, ()>(2)
                })
                .await
        });
        tokio::task::yield_now().await;
        drop(held);
        assert_eq!(
            *waiter.await.expect("waiter join").expect("waiter lease"),
            1
        );
    }

    #[tokio::test]
    async fn dedicated_stream_ownership_holds_capacity_until_drop() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 1,
            max_connections_per_endpoint: 1,
            max_idle_per_endpoint: 1,
        });
        let dedicated = pool
            .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(1)
            })
            .await
            .expect("lease")
            .into_dedicated();
        assert_eq!(pool.counts(), (1, 1, 0));
        let error = pool
            .acquire("a", Instant::now() + Duration::from_millis(10), || async {
                Ok::<_, ()>(2)
            })
            .await
            .expect_err("dedicated lease keeps capacity charged");
        assert!(matches!(
            error,
            ConnectionPoolAcquireError::AdmissionTimedOut
        ));
        drop(dedicated);
        assert_eq!(pool.counts(), (0, 0, 0));
    }

    #[tokio::test]
    async fn dedicated_conversion_retains_capacity_without_leaking_accounting() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 1,
            max_connections_per_endpoint: 1,
            max_idle_per_endpoint: 1,
        });
        let dedicated = pool
            .acquire("a", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(41_usize)
            })
            .await
            .expect("lease")
            .into_dedicated()
            .try_map(|value| Ok::<_, ()>(value.to_string()))
            .expect("convert dedicated value");
        assert_eq!(dedicated.as_str(), "41");
        assert_eq!(pool.counts(), (1, 1, 0));
        drop(dedicated);
        assert_eq!(pool.counts(), (0, 0, 0));
    }

    #[tokio::test]
    async fn idle_other_endpoint_is_evicted_for_new_endpoint() {
        let pool = pool(ConnectionPoolLimits {
            max_connections: 1,
            max_connections_per_endpoint: 1,
            max_idle_per_endpoint: 1,
        });
        drop(
            pool.acquire("a", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(1)
            })
            .await
            .expect("first endpoint"),
        );
        let lease = pool
            .acquire("b", Instant::now() + Duration::from_secs(1), || async {
                Ok::<_, ()>(2)
            })
            .await
            .expect("second endpoint");
        assert_eq!(*lease, 2);
    }
}
