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
    PromptEvent, PromptField, PromptFormField, PromptFormFieldKind, PromptFormSection,
    PromptFormValue, PromptOption, PromptPolicy, PromptRequest, PromptResponse,
    PromptSearchMatchMode, PromptValidation, PromptValue, PromptWidth,
};

// ── Host request envelope ────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PromptHostRequest {
    pub request: PromptRequest,
    pub response_tx: oneshot::Sender<PromptResponse>,
    pub event_tx: Option<mpsc::UnboundedSender<PromptEvent>>,
}

// ── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSubmitError {
    HostUnavailable,
    HostDisconnected,
    Full,
    TooLarge,
    OwnerClosed,
}

impl fmt::Display for PromptSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => f.write_str("prompt host unavailable"),
            Self::HostDisconnected => f.write_str("prompt host disconnected"),
            Self::Full => f.write_str("prompt queue full"),
            Self::TooLarge => f.write_str("prompt exceeds size bound"),
            Self::OwnerClosed => f.write_str("prompt owner closed"),
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
    submit_inner(request, None)
}

/// Submit a prompt request and receive live prompt events.
///
/// # Errors
///
/// Returns [`PromptSubmitError::HostUnavailable`] if no host is registered,
/// or [`PromptSubmitError::HostDisconnected`] if the channel is closed.
pub fn submit_with_events(
    request: PromptRequest,
) -> std::result::Result<
    (
        oneshot::Receiver<PromptResponse>,
        mpsc::UnboundedReceiver<PromptEvent>,
    ),
    PromptSubmitError,
> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let response_rx = submit_inner(request, Some(event_tx))?;
    Ok((response_rx, event_rx))
}

fn submit_inner(
    request: PromptRequest,
    event_tx: Option<mpsc::UnboundedSender<PromptEvent>>,
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
            event_tx,
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

/// Submit a prompt request and receive response and event receivers.
///
/// # Errors
///
/// Returns [`PromptSubmitError::HostUnavailable`] if no host is registered,
/// or [`PromptSubmitError::HostDisconnected`] if the channel is closed.
pub fn request_with_events(
    request: PromptRequest,
) -> std::result::Result<
    (
        oneshot::Receiver<PromptResponse>,
        mpsc::UnboundedReceiver<PromptEvent>,
    ),
    PromptSubmitError,
> {
    submit_with_events(request)
}

/// Explicit bounded frontend prompt port. Keep one port/inbox per activation;
/// clones share only that owner's channel, never a process-global host slot.
#[derive(Clone)]
pub struct PromptPort {
    owner: bmux_plugin_runtime::ActivationScope,
    sender: mpsc::Sender<ScopedPromptRequest>,
}

pub struct PromptInbox {
    owner: bmux_plugin_runtime::ActivationScope,
    receiver: mpsc::Receiver<ScopedPromptRequest>,
}

struct ScopedPromptRequest {
    request: PromptRequest,
    response: oneshot::Sender<PromptResponse>,
    events: Option<mpsc::Sender<PromptEvent>>,
}

/// Retains lifecycle admission while a displayed prompt awaits completion.
/// Responding consumes it; dropping it disconnects the requesting caller.
pub struct AdmittedPrompt {
    owner: bmux_plugin_runtime::ActivationScope,
    request: ScopedPromptRequest,
    _admission: bmux_plugin_runtime::AdmissionGuard,
}

// Validate encoded size without allocating a second unbounded payload buffer.
struct PromptSizeBudget(usize);

impl std::io::Write for PromptSizeBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("prompt payload exceeds bound"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn validate_prompt_size(value: &impl serde::Serialize) -> Result<(), PromptSubmitError> {
    serde_json::to_writer(PromptSizeBudget(1024 * 1024), value)
        .map_err(|_| PromptSubmitError::TooLarge)
}

impl PromptPort {
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn channel(
        owner: bmux_plugin_runtime::ActivationScope,
        capacity: usize,
    ) -> Result<(Self, PromptInbox), PromptSubmitError> {
        if !(1..=1024).contains(&capacity) {
            return Err(PromptSubmitError::Full);
        }
        let _admission = owner
            .try_admit()
            .map_err(|_| PromptSubmitError::OwnerClosed)?;
        let (sender, receiver) = mpsc::channel(capacity);
        Ok((
            Self {
                owner: owner.clone(),
                sender,
            },
            PromptInbox { owner, receiver },
        ))
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn submit(
        &self,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<PromptResponse>, PromptSubmitError> {
        self.submit_with_events(request, None)
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn submit_with_events(
        &self,
        request: PromptRequest,
        events: Option<mpsc::Sender<PromptEvent>>,
    ) -> Result<oneshot::Receiver<PromptResponse>, PromptSubmitError> {
        validate_prompt_size(&request)?;
        let _admission = self
            .owner
            .try_admit()
            .map_err(|_| PromptSubmitError::OwnerClosed)?;
        let (response, receiver) = oneshot::channel();
        self.sender
            .try_send(ScopedPromptRequest {
                request,
                response,
                events,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => PromptSubmitError::Full,
                mpsc::error::TrySendError::Closed(_) => PromptSubmitError::HostDisconnected,
            })?;
        Ok(receiver)
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub async fn request(
        &self,
        request: PromptRequest,
    ) -> Result<PromptResponse, PromptSubmitError> {
        let response = self.submit(request)?;
        tokio::select! {
            _ = self.owner.cancelled() => Err(PromptSubmitError::OwnerClosed),
            response = response => response.map_err(|_| PromptSubmitError::HostDisconnected),
        }
    }
}

impl PromptInbox {
    /// Drain a single queued prompt without suspending the frame loop. Cancelled
    /// requests are skipped; work is bounded by the channel's maximum capacity.
    ///
    /// # Errors
    /// Rejects closed owners and disconnected senders; empty is `Ok(None)`.
    pub fn try_recv(&mut self) -> Result<Option<AdmittedPrompt>, PromptSubmitError> {
        for _ in 0..1024 {
            let admission = self
                .owner
                .try_admit()
                .map_err(|_| PromptSubmitError::OwnerClosed)?;
            match self.receiver.try_recv() {
                Ok(request) if request.response.is_closed() => {}
                Ok(request) => {
                    return Ok(Some(AdmittedPrompt {
                        owner: self.owner.clone(),
                        request,
                        _admission: admission,
                    }));
                }
                Err(mpsc::error::TryRecvError::Empty) => return Ok(None),
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(PromptSubmitError::HostDisconnected);
                }
            }
        }
        Ok(None)
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub async fn recv(&mut self) -> Result<AdmittedPrompt, PromptSubmitError> {
        loop {
            let request = tokio::select! {
                _ = self.owner.cancelled() => {
                    self.receiver.close();
                    return Err(PromptSubmitError::OwnerClosed);
                }
                request = self.receiver.recv() => request.ok_or(PromptSubmitError::HostDisconnected)?,
            };
            if request.response.is_closed() {
                continue;
            }
            let admission = self
                .owner
                .try_admit()
                .map_err(|_| PromptSubmitError::OwnerClosed)?;
            return Ok(AdmittedPrompt {
                owner: self.owner.clone(),
                request,
                _admission: admission,
            });
        }
    }
}

impl AdmittedPrompt {
    /// Host removes cancelled prompts rather than keeping background modal input
    /// targets after their requesting caller or activation has gone away.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.request.response.is_closed() || self.owner.is_cancelled().unwrap_or(true)
    }

    #[must_use]
    pub const fn request(&self) -> &PromptRequest {
        &self.request.request
    }

    /// Full event queues are explicit failures, not silently lost interaction.
    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn emit(&self, event: PromptEvent) -> Result<(), PromptSubmitError> {
        if self
            .owner
            .is_cancelled()
            .map_err(|_| PromptSubmitError::OwnerClosed)?
        {
            return Err(PromptSubmitError::OwnerClosed);
        }
        validate_prompt_size(&event)?;
        let Some(events) = &self.request.events else {
            return Ok(());
        };
        events.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => PromptSubmitError::Full,
            mpsc::error::TrySendError::Closed(_) => PromptSubmitError::HostDisconnected,
        })
    }

    ///
    /// # Errors
    /// Returns an explicit error for unavailable/closed owners, invalid input,
    /// capacity limits, or failed dispatch/cleanup; no alternate host is selected.
    pub fn respond(self, response: PromptResponse) -> Result<(), PromptSubmitError> {
        if self
            .owner
            .is_cancelled()
            .map_err(|_| PromptSubmitError::OwnerClosed)?
        {
            return Err(PromptSubmitError::OwnerClosed);
        }
        validate_prompt_size(&response)?;
        self.request
            .response
            .send(response)
            .map_err(|_| PromptSubmitError::HostDisconnected)
    }
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
    async fn scoped_prompt_is_bounded_and_does_not_cross_hosts() {
        use bmux_plugin_runtime::{InstanceScope, PluginConcurrencyConfig, PluginRuntimeScopeId};
        let instance = InstanceScope::new(PluginRuntimeScopeId::from_uuid(uuid::Uuid::new_v4()));
        let owner = instance
            .begin_activation(PluginConcurrencyConfig::Concurrent)
            .unwrap();
        owner.mark_active().unwrap();
        let (port, mut inbox) = PromptPort::channel(owner.clone(), 1).unwrap();
        let response = port.submit(PromptRequest::confirm("one")).unwrap();
        assert!(matches!(
            port.submit(PromptRequest::confirm("two")),
            Err(PromptSubmitError::Full)
        ));
        let prompt = inbox.recv().await.unwrap();
        assert_eq!(prompt.request().title, "one");
        prompt.respond(PromptResponse::Cancelled).unwrap();
        assert_eq!(response.await.unwrap(), PromptResponse::Cancelled);
        let response = port.submit(PromptRequest::confirm("stale")).unwrap();
        owner.close().unwrap();
        assert!(matches!(
            inbox.recv().await,
            Err(PromptSubmitError::OwnerClosed)
        ));
        assert!(matches!(
            port.submit(PromptRequest::confirm("closed")),
            Err(PromptSubmitError::OwnerClosed)
        ));
        drop(inbox);
        assert!(response.await.is_err());
        owner.drain().await.unwrap();
        owner.finish().unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn response_channel_disconnect_is_reported() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let _guard = register_host(tx);
        let response_rx = submit(PromptRequest::confirm("disconnect")).unwrap();
        let host_request = rx.recv().await.expect("host request");
        drop(host_request.response_tx);

        let error = response_rx.await.expect_err("disconnect closes response");
        assert!(error.to_string().contains("channel closed"));
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
