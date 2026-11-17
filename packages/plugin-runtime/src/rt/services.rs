//! Asynchronous host service calls across the plug-rt boundary.
//!
//! A dynamic plugin's synchronous host route blocks its thread inside the
//! host callback. The asynchronous route instead starts the call as a host
//! task owned by the plugin instance's scope and lets the plugin await its
//! outcome with a real waker: the host wakes the plugin when the outcome is
//! ready, nothing polls on a timer. Dropping the plugin-side future cancels
//! the call (the host future is dropped), and scope teardown aborts calls
//! still in flight.
//!
//! The call is made through one invocation port, which carries its
//! authority; this module only moves the encoded request and outcome. The
//! SDK owns their encoding and the host resolves ports ([`PortServices`]).
use super::abi::{PlugRtV1, RT_PENDING, RT_READY, RT_TOO_SMALL, RtStatus};
use super::waker;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A host service call's encoded outcome, produced by the host.
pub type ServiceOutcome = Pin<Box<dyn Future<Output = Vec<u8>> + Send>>;

/// Host resolution of invocation ports to asynchronous service calls.
pub trait PortServices: Send + Sync {
    /// Start one call through invocation port `port`. Refusals (closed
    /// port, cancelled invocation, unsupported route) are encoded in the
    /// outcome like any other failure.
    fn call(&self, port: u64, request: Vec<u8>) -> ServiceOutcome;
}

/// Start a call through `port` in the instance scope entered on this thread,
/// when this image is a dynamic plugin with an installed host table.
#[must_use]
pub fn call_current(port: u64, request: &[u8]) -> Option<ServiceCall> {
    if !super::is_dynamic_build() {
        return None;
    }
    let scope = super::scope::current_remote();
    super::guest::installed().map(|client| client.service_call(scope, port, request))
}

/// Plugin-side future of one asynchronous host service call.
pub struct ServiceCall {
    table: &'static PlugRtV1,
    id: Option<u64>,
    matched: bool,
    refused: Option<RtStatus>,
    buffer: Vec<u8>,
}
impl fmt::Debug for ServiceCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceCall")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Why the plug-rt route produced no outcome bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceCallRefused(pub RtStatus);
impl fmt::Display for ServiceCallRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "host refused the asynchronous service call (status {})",
            self.0
        )
    }
}
impl std::error::Error for ServiceCallRefused {}

impl ServiceCall {
    pub(crate) fn start(
        table: &'static PlugRtV1,
        matched: bool,
        scope: u64,
        port: u64,
        request: &[u8],
    ) -> Self {
        let mut id = 0;
        let status = unsafe {
            (table.service_call)(
                table.host,
                scope,
                port,
                request.as_ptr(),
                request.len(),
                &raw mut id,
            )
        };
        let accepted = status == RT_READY;
        Self {
            table,
            id: accepted.then_some(id),
            matched,
            refused: (!accepted).then_some(status),
            buffer: vec![0; 4096],
        }
    }
}

impl Future for ServiceCall {
    type Output = Result<Vec<u8>, ServiceCallRefused>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(status) = this.refused {
            return Poll::Ready(Err(ServiceCallRefused(status)));
        }
        let Some(id) = this.id else {
            return Poll::Ready(Err(ServiceCallRefused(super::abi::RT_CLOSED)));
        };
        loop {
            let mut length = 0;
            let table = this.table;
            let buffer = &mut this.buffer;
            let status = waker::with_cx(cx.waker(), this.matched, |rtcx| unsafe {
                (table.service_poll)(
                    table.host,
                    id,
                    rtcx,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                    &raw mut length,
                )
            });
            match status {
                RT_READY => {
                    // The host released the finished call.
                    this.id = None;
                    return Poll::Ready(Ok(this.buffer[..length].to_vec()));
                }
                RT_PENDING => return Poll::Pending,
                RT_TOO_SMALL => this.buffer.resize(length, 0),
                other => {
                    this.id = None;
                    return Poll::Ready(Err(ServiceCallRefused(other)));
                }
            }
        }
    }
}

impl Drop for ServiceCall {
    fn drop(&mut self) {
        // An unfinished call is cancelled: the host drops its future.
        if let Some(id) = self.id.take() {
            unsafe { (self.table.service_cancel)(self.table.host, id) };
        }
    }
}
