//! Prompt host registration and async submission.
//!
//! The prompt **data types** are defined in [`bmux_plugin_sdk::prompt`].
//! This module adds the process-global host channel that connects prompt
//! callers (plugins, async tasks) to the attach loop.
//!
//! The attach loop registers itself as the host on startup via
//! [`register_host`].  Plugin code submits prompts via [`submit`] or the
//! async [`request`].

use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::sync::{mpsc, oneshot};

pub use bmux_plugin_sdk::prompt::{
    PromptField, PromptOption, PromptPolicy, PromptRequest, PromptResponse, PromptValidation,
    PromptValue, PromptWidth,
};

// ── Host request envelope ────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PromptHostRequest {
    pub request: PromptRequest,
    pub response_tx: oneshot::Sender<PromptResponse>,
}

// ── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSubmitError {
    HostUnavailable,
    HostDisconnected,
}

impl fmt::Display for PromptSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => f.write_str("prompt host unavailable"),
            Self::HostDisconnected => f.write_str("prompt host disconnected"),
        }
    }
}

impl Error for PromptSubmitError {}

// ── Host registration ────────────────────────────────────────────────────────

#[derive(Clone)]
struct PromptHostRegistration {
    id: u64,
    sender: mpsc::UnboundedSender<PromptHostRequest>,
}

static HOST_REGISTRY: OnceLock<Mutex<Option<PromptHostRegistration>>> = OnceLock::new();
static HOST_REGISTRATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn host_registry() -> &'static Mutex<Option<PromptHostRegistration>> {
    HOST_REGISTRY.get_or_init(|| Mutex::new(None))
}

#[derive(Debug)]
pub struct PromptHostGuard {
    id: u64,
}

impl Drop for PromptHostGuard {
    fn drop(&mut self) {
        if let Ok(mut slot) = host_registry().lock()
            && slot
                .as_ref()
                .is_some_and(|registration| registration.id == self.id)
        {
            *slot = None;
        }
    }
}

/// Register the attach loop as the prompt host.
///
/// Only one host can be registered at a time.  Dropping the returned guard
/// unregisters the host.
pub fn register_host(sender: mpsc::UnboundedSender<PromptHostRequest>) -> PromptHostGuard {
    let id = HOST_REGISTRATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut slot) = host_registry().lock() {
        *slot = Some(PromptHostRegistration { id, sender });
    }
    PromptHostGuard { id }
}

/// Submit a prompt request and receive a oneshot receiver for the response.
///
/// # Errors
///
/// Returns [`PromptSubmitError::HostUnavailable`] if no host is registered,
/// or [`PromptSubmitError::HostDisconnected`] if the channel is closed.
pub fn submit(
    request: PromptRequest,
) -> std::result::Result<oneshot::Receiver<PromptResponse>, PromptSubmitError> {
    let guard = host_registry()
        .lock()
        .map_err(|_| PromptSubmitError::HostDisconnected)?;
    let sender = guard
        .as_ref()
        .map(|registration| registration.sender.clone())
        .ok_or(PromptSubmitError::HostUnavailable)?;
    drop(guard);

    let (response_tx, response_rx) = oneshot::channel();
    sender
        .send(PromptHostRequest {
            request,
            response_tx,
        })
        .map_err(|_| PromptSubmitError::HostDisconnected)?;
    Ok(response_rx)
}

/// Submit a prompt request and wait for the response.
///
/// # Errors
///
/// Returns [`PromptSubmitError::HostUnavailable`] if no host is registered,
/// or [`PromptSubmitError::HostDisconnected`] if the channel is closed or
/// the host drops the response sender.
pub async fn request(
    request: PromptRequest,
) -> std::result::Result<PromptResponse, PromptSubmitError> {
    let response_rx = submit(request)?;
    response_rx
        .await
        .map_err(|_| PromptSubmitError::HostDisconnected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn request_fails_when_no_host_is_registered() {
        let response = request(PromptRequest::confirm("missing host")).await;
        assert_eq!(response, Err(PromptSubmitError::HostUnavailable));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn request_routes_through_registered_host() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _guard = register_host(tx);

        let client_task =
            tokio::spawn(async { request(PromptRequest::confirm("quit session?")).await });

        let host_request = rx.recv().await.expect("host should receive request");
        assert_eq!(host_request.request.title, "quit session?");
        host_request
            .response_tx
            .send(PromptResponse::Cancelled)
            .expect("host should send response");

        let response = client_task
            .await
            .expect("request task should complete")
            .expect("request should resolve");
        assert_eq!(response, PromptResponse::Cancelled);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn dropping_host_guard_unregisters_the_host() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let guard = register_host(tx);
        drop(guard);

        let receiver = submit(PromptRequest::confirm("hello"));
        assert!(matches!(receiver, Err(PromptSubmitError::HostUnavailable)));
    }
}
