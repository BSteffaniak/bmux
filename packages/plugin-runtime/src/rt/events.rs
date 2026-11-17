//! Serialized activation events across the plug-rt boundary.
//!
//! In-process plugins receive their activation's event view as a typed value.
//! A dynamic library cannot: its copy of every type is distinct. The host
//! therefore binds a serialized event port to each instance scope
//! ([`ScopeEvents`], through `PlugRtHost::bind_scope_events`), and the
//! plugin side reaches it with [`RemoteEvents`]. Payloads are JSON bytes;
//! channel keys are plain event-kind strings. Subscriptions are host
//! resources owned by the instance scope: the plugin polls them with its
//! real waker (no polling loop), and they are released when the plugin
//! drops them or the host unbinds the scope at teardown.
//!
//! The host decides what a kind means; this module only moves bytes.
use super::abi::{
    PlugRtV1, RT_CLOSED, RT_PENDING, RT_READY, RT_TOO_SMALL, RT_UNSUPPORTED, RtStatus, RtStr,
};
use super::waker;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// Why an event-port operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPortError {
    /// The owning activation scope closed (teardown began).
    Closed,
    /// The host bound no event port to this instance.
    Unsupported,
    /// The host refused the operation (unknown channel, wrong delivery
    /// mode, undecodable payload, ...).
    Rejected(String),
}
impl fmt::Display for EventPortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("activation event scope is closed"),
            Self::Unsupported => formatter.write_str("host supplied no activation events"),
            Self::Rejected(reason) => write!(formatter, "event port rejected: {reason}"),
        }
    }
}
impl std::error::Error for EventPortError {}

/// Host implementation of one instance scope's serialized event port.
pub trait ScopeEvents: Send + Sync {
    /// Register a JSON channel owned by the scope's activation.
    ///
    /// # Errors
    /// The scope closed or the host refused the registration.
    fn register(&self, kind: &str) -> Result<(), EventPortError>;
    /// Publish one JSON payload.
    ///
    /// # Errors
    /// The scope closed or the channel does not accept serialized input.
    fn publish(&self, kind: &str, payload: &[u8]) -> Result<(), EventPortError>;
    /// Subscribe to a broadcast (`state == false`) or state channel. Items
    /// are serialized events; a state stream yields the current value first.
    ///
    /// # Errors
    /// The scope closed or the channel cannot be observed serialized.
    fn subscribe(&self, kind: &str, state: bool) -> Result<Box<dyn EventStream>, EventPortError>;
}

/// A host-side stream of serialized events. `None` ends the stream.
pub trait EventStream: Send {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>>;
}

type Pending<R, T> = Pin<Box<dyn Future<Output = (Option<T>, R)> + Send>>;

/// An [`EventStream`] over any receiver whose next item is produced by an
/// async step. The step future is kept across polls, so the receiver's waker
/// registration is never dropped between wakes.
struct Stepped<R, T, S, E> {
    step: S,
    encode: E,
    pending: Option<Pending<R, T>>,
    receiver: Option<R>,
}
impl<R, T, S, Fut, E> EventStream for Stepped<R, T, S, E>
where
    R: Send + 'static,
    T: Send + 'static,
    S: Fn(R) -> Fut + Send,
    Fut: Future<Output = (Option<T>, R)> + Send + 'static,
    E: Fn(T) -> Option<Vec<u8>> + Send,
{
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        loop {
            if self.pending.is_none() {
                let Some(receiver) = self.receiver.take() else {
                    return Poll::Ready(None);
                };
                self.pending = Some(Box::pin((self.step)(receiver)));
            }
            let Some(pending) = self.pending.as_mut() else {
                return Poll::Ready(None);
            };
            let (item, receiver) = match pending.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(done) => done,
            };
            self.pending = None;
            let Some(item) = item else {
                return Poll::Ready(None);
            };
            self.receiver = Some(receiver);
            if let Some(bytes) = (self.encode)(item) {
                return Poll::Ready(Some(bytes));
            }
            // Unencodable item: skip it and wait for the next one.
        }
    }
}

/// Serve a broadcast receiver as an [`EventStream`]. Items a lagging
/// subscriber missed are skipped (broadcast semantics); a closed channel
/// ends the stream.
pub fn broadcast_stream<T, E>(
    receiver: tokio::sync::broadcast::Receiver<T>,
    encode: E,
) -> Box<dyn EventStream>
where
    T: Clone + Send + 'static,
    E: Fn(T) -> Option<Vec<u8>> + Send + 'static,
{
    Box::new(Stepped {
        step: |mut receiver: tokio::sync::broadcast::Receiver<T>| async move {
            loop {
                match receiver.recv().await {
                    Ok(item) => return (Some(item), receiver),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return (None, receiver);
                    }
                }
            }
        },
        encode,
        pending: None,
        receiver: Some(receiver),
    })
}

/// Serve a watch receiver as an [`EventStream`]: the current value first,
/// then every change. A closed channel ends the stream.
pub fn watch_stream<T, E>(
    receiver: tokio::sync::watch::Receiver<T>,
    encode: E,
) -> Box<dyn EventStream>
where
    T: Clone + Send + Sync + 'static,
    E: Fn(T) -> Option<Vec<u8>> + Send + 'static,
{
    let mut receiver = receiver;
    receiver.mark_changed();
    Box::new(Stepped {
        step: |mut receiver: tokio::sync::watch::Receiver<T>| async move {
            match receiver.changed().await {
                Ok(()) => {
                    let value = receiver.borrow_and_update().clone();
                    (Some(value), receiver)
                }
                Err(_) => (None, receiver),
            }
        },
        encode,
        pending: None,
        receiver: Some(receiver),
    })
}

/// Plugin-side handle to the event port of one instance scope.
#[derive(Clone)]
pub struct RemoteEvents {
    table: &'static PlugRtV1,
    scope: u64,
    matched: bool,
}
impl fmt::Debug for RemoteEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteEvents")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

fn port_status(status: RtStatus, what: &str) -> Result<(), EventPortError> {
    match status {
        RT_READY => Ok(()),
        RT_CLOSED => Err(EventPortError::Closed),
        RT_UNSUPPORTED => Err(EventPortError::Unsupported),
        other => Err(EventPortError::Rejected(format!("{what} (status {other})"))),
    }
}

impl RemoteEvents {
    pub(crate) const fn new(table: &'static PlugRtV1, scope: u64, matched: bool) -> Self {
        Self {
            table,
            scope,
            matched,
        }
    }

    /// The event port of the instance scope entered on this thread, when
    /// this image is a dynamic plugin with an installed host table. Bundled
    /// plugins receive their event view in-process instead.
    #[must_use]
    pub fn current() -> Option<Self> {
        if !super::is_dynamic_build() {
            return None;
        }
        let scope = super::scope::current_remote();
        if scope == 0 {
            return None;
        }
        super::guest::installed().map(|client| client.events(scope))
    }

    /// The instance scope this port belongs to.
    #[must_use]
    pub const fn scope(&self) -> u64 {
        self.scope
    }

    /// See [`ScopeEvents::register`].
    ///
    /// # Errors
    /// As reported by the host.
    pub fn register(&self, kind: &str) -> Result<(), EventPortError> {
        port_status(
            unsafe { (self.table.event_register)(self.table.host, self.scope, RtStr::new(kind)) },
            "register",
        )
    }

    /// See [`ScopeEvents::publish`].
    ///
    /// # Errors
    /// As reported by the host.
    pub fn publish(&self, kind: &str, payload: &[u8]) -> Result<(), EventPortError> {
        port_status(
            unsafe {
                (self.table.event_publish)(
                    self.table.host,
                    self.scope,
                    RtStr::new(kind),
                    payload.as_ptr(),
                    payload.len(),
                )
            },
            "publish",
        )
    }

    /// See [`ScopeEvents::subscribe`].
    ///
    /// # Errors
    /// As reported by the host.
    pub fn subscribe(&self, kind: &str, state: bool) -> Result<RemoteSubscription, EventPortError> {
        let mut id = 0;
        port_status(
            unsafe {
                (self.table.event_subscribe)(
                    self.table.host,
                    self.scope,
                    RtStr::new(kind),
                    u32::from(state),
                    &raw mut id,
                )
            },
            "subscribe",
        )?;
        Ok(RemoteSubscription {
            table: self.table,
            id,
            matched: self.matched,
            buffer: vec![0; 4096],
        })
    }
}

/// A host subscription seen from the plugin. Dropping it releases it.
pub struct RemoteSubscription {
    table: &'static PlugRtV1,
    id: u64,
    matched: bool,
    buffer: Vec<u8>,
}
impl fmt::Debug for RemoteSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteSubscription")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl RemoteSubscription {
    /// Poll the next serialized event with the caller's waker.
    pub fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        loop {
            let mut length = 0;
            let table = self.table;
            let id = self.id;
            let buffer = &mut self.buffer;
            let status = waker::with_cx(cx.waker(), self.matched, |rtcx| unsafe {
                (table.event_poll)(
                    table.host,
                    id,
                    rtcx,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &raw mut length,
                )
            });
            match status {
                RT_READY => return Poll::Ready(Some(self.buffer[..length].to_vec())),
                RT_PENDING => return Poll::Pending,
                // The host retained the item; grow and copy it now.
                RT_TOO_SMALL => self.buffer.resize(length, 0),
                _ => return Poll::Ready(None),
            }
        }
    }

    /// The next serialized event, or `None` once the stream ended (the
    /// channel went away or the scope was released).
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        std::future::poll_fn(|cx| self.poll_next(cx)).await
    }
}
impl Drop for RemoteSubscription {
    fn drop(&mut self) {
        unsafe { (self.table.event_unsubscribe)(self.table.host, self.id) };
    }
}
