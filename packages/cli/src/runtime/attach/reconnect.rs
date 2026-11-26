//! Reconnecting an attachment in place.
//!
//! When the connection to the server is lost, the attach client keeps the
//! terminal in attach mode, keeps the last frame on screen under a
//! [`ConnectionBanner::Reconnecting`] banner, and reconnects with backoff
//! ([`reconnect_in_place`]) through the connector the attachment was started
//! with ([`AttachReconnect`]). Each attempt connects, then re-admits the
//! attachment (the caller's `reattach`); the attach loop then re-registers
//! what it holds with the server and rehydrates the view.

use super::connection_banner::{BannerAction, ConnectionBanner};
use super::state::AttachExitReason;
use crate::runtime::KernelClientFactory;
use bmux_client::BmuxClient;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// How long to wait before each reconnect attempt: doubling from `base` up to
/// `max`, spread by up to `jitter_permille` thousandths either way so clients
/// that lost the same server do not all come back at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectBackoff {
    base: Duration,
    max: Duration,
    jitter_permille: u64,
}

impl ReconnectBackoff {
    /// The attach client's in-place reconnect: 300ms doubling to 5s, ±20%.
    pub const ATTACH: Self = Self {
        base: Duration::from_millis(300),
        max: Duration::from_secs(5),
        jitter_permille: 200,
    };

    /// A schedule without jitter (tests time it exactly).
    #[cfg(test)]
    #[must_use]
    pub const fn fixed(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            jitter_permille: 0,
        }
    }

    /// The delay before attempt `attempt` (from 1); `seed` picks the jitter.
    #[must_use]
    pub fn delay(self, attempt: u32, seed: u64) -> Duration {
        let exponent = attempt.saturating_sub(1).min(20);
        let nominal = self.base.saturating_mul(1 << exponent).min(self.max);
        if self.jitter_permille == 0 {
            return nominal;
        }
        let spread = splitmix64(seed ^ u64::from(attempt)) % (2 * self.jitter_permille + 1);
        let nominal_ms = u64::try_from(nominal.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(
            nominal_ms.saturating_mul(1000 - self.jitter_permille + spread) / 1000,
        )
    }
}

const fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// A jitter seed that differs between processes and runs.
#[must_use]
pub fn jitter_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    splitmix64(u64::from(nanos) ^ (u64::from(std::process::id()) << 32))
}

/// One reconnect attempt, as its connector sees it.
#[derive(Clone, Copy, Debug)]
pub struct AttachReconnectAttempt {
    /// Attempts since the connection was lost, this one included.
    pub number: u32,
    /// How long the connection has been lost.
    pub outage: Duration,
    /// How long this attempt waited before connecting.
    pub backoff: Duration,
    /// The server said it was stopping before the connection dropped: a
    /// connector that could start a server only waits for one.
    pub server_stopped: bool,
}

/// Marks a connector failure that retrying cannot fix (attach it with
/// `error.context(ReconnectGiveUp)`): reconnecting stops, the attachment ends
/// as [`AttachExitReason::ConnectionLost`].
#[derive(Clone, Copy, Debug)]
pub struct ReconnectGiveUp;

impl std::fmt::Display for ReconnectGiveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("reconnecting cannot succeed")
    }
}

/// A connector's connect failure as the reconnect loop sees it: marked
/// [`ReconnectGiveUp`] when the host refused this device itself (the
/// saved-host connector reports a revoked or unpaired device as an I/O
/// `PermissionDenied`), since no retry can succeed.
#[must_use]
pub fn connector_connect_error(error: bmux_client::ClientError) -> anyhow::Error {
    let refused = matches!(
        &error,
        bmux_client::ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(io))
            if io.kind() == std::io::ErrorKind::PermissionDenied
    );
    let error = crate::connection::map_client_connect_error(error);
    if refused {
        error.context(ReconnectGiveUp)
    } else {
        error
    }
}

/// Reconnect attempts of a connector-backed (remote) attachment before it
/// gives up: with the attach backoff, at least two minutes of trying (longer
/// while dials to an unreachable host wait out their own timeouts), which
/// outlasts a typical network change.
pub const CONNECTOR_RECONNECT_MAX_ATTEMPTS: u32 = 34;

/// A new connection for the attachment.
pub struct AttachConnection {
    pub client: BmuxClient,
    /// The kernel client factory for this connection, when the attachment's
    /// plugin commands need one bound to it (otherwise the old one is kept).
    pub kernel_client_factory: Option<KernelClientFactory>,
}

impl From<BmuxClient> for AttachConnection {
    fn from(client: BmuxClient) -> Self {
        Self {
            client,
            kernel_client_factory: None,
        }
    }
}

pub type AttachConnectFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<AttachConnection>> + Send>>;
type AttachConnector = Arc<dyn Fn(AttachReconnectAttempt) -> AttachConnectFuture + Send + Sync>;

/// When reconnect attempts run, and how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// `None`: until the user quits.
    pub max_attempts: Option<u32>,
    pub backoff: ReconnectBackoff,
    /// The other side asked to wait this long before the first attempt
    /// (instead of the first backoff).
    pub first_delay: Option<Duration>,
    /// How many attempts in a row must find the session gone before the
    /// attachment ends: a server that has just come back may answer before
    /// it has restored its sessions, so one answer is not proof.
    pub gone_confirmations: u32,
}

impl ReconnectPolicy {
    /// Attempts until the user quits, on the attach backoff.
    pub const UNTIL_QUIT: Self = Self {
        max_attempts: None,
        backoff: ReconnectBackoff::ATTACH,
        first_delay: None,
        gone_confirmations: 2,
    };

    fn delay(&self, attempt: u32, seed: u64) -> Duration {
        match self.first_delay {
            Some(delay) if attempt == 1 => delay,
            _ => self.backoff.delay(attempt, seed),
        }
    }
}

/// How an attachment reconnects after its connection is lost: the same
/// connection path it was started with.
#[derive(Clone)]
pub struct AttachReconnect {
    connect: AttachConnector,
    policy: ReconnectPolicy,
}

impl AttachReconnect {
    /// Reconnect with `connect`, until the user quits.
    pub fn new<F, Fut>(connect: F) -> Self
    where
        F: Fn(AttachReconnectAttempt) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<AttachConnection>> + Send + 'static,
    {
        Self {
            connect: Arc::new(move |attempt| Box::pin(connect(attempt))),
            policy: ReconnectPolicy::UNTIL_QUIT,
        }
    }

    /// Give up after `max_attempts` failed attempts (`None`: never).
    #[must_use]
    pub const fn with_max_attempts(mut self, max_attempts: Option<u32>) -> Self {
        self.policy.max_attempts = max_attempts;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub const fn with_policy(mut self, policy: ReconnectPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub const fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// Connect for `attempt`.
    pub fn connect(&self, attempt: AttachReconnectAttempt) -> AttachConnectFuture {
        (self.connect)(attempt)
    }
}

pub type ProviderReopenFuture =
    Pin<Box<dyn Future<Output = anyhow::Result<Box<dyn bmux_client::AttachSession>>> + Send>>;
type ProviderReopener =
    Arc<dyn Fn(Option<bmux_client::AttachResumeState>) -> ProviderReopenFuture + Send + Sync>;

/// How a native provider attachment reopens its provider after the
/// connection to it is lost: from the view it validated so far, when it can
/// resume (`Some`), else fresh.
#[derive(Clone)]
pub struct ProviderReopen {
    open: ProviderReopener,
    policy: ReconnectPolicy,
}

impl ProviderReopen {
    pub fn new<F, Fut>(open: F) -> Self
    where
        F: Fn(Option<bmux_client::AttachResumeState>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<Box<dyn bmux_client::AttachSession>>> + Send + 'static,
    {
        Self {
            open: Arc::new(move |resume| Box::pin(open(resume))),
            policy: ReconnectPolicy::UNTIL_QUIT,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub const fn with_policy(mut self, policy: ReconnectPolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub const fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// Reopen the provider, resuming from `resume` when it is `Some`.
    pub fn open(&self, resume: Option<bmux_client::AttachResumeState>) -> ProviderReopenFuture {
        (self.open)(resume)
    }
}

/// What reconnecting ended with.
pub enum ReconnectEnd<T> {
    /// The attachment is back, re-admitted as `T`.
    Reattached(T),
    /// The attachment ends this way (the user left, the session is gone, or
    /// the attempts ran out).
    Exit(AttachExitReason),
}

/// Why one attempt to re-admit the attachment failed.
pub enum ReattachFailure {
    /// Try again later.
    Retry(anyhow::Error),
    /// The server is back, but the attached session (or followed client) is
    /// not.
    Gone,
}

/// Terminal input while reconnecting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconnectInput {
    /// A plain key press.
    Key(char),
    /// The screen changed size or was uncovered: paint the banner again.
    Repaint,
    /// The terminal went away; the attachment ends this way.
    Closed(AttachExitReason),
    /// Nothing the banner answers (dropped: there is nothing to send it to).
    Ignored,
}

/// The terminal side of reconnecting: its input, and where the banner shows.
pub trait ReconnectHost {
    fn next_input(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReconnectInput>> + Send + '_>>;

    /// Show `banner` over what the terminal shows.
    ///
    /// # Errors
    /// Returns the terminal write failure.
    fn show(&mut self, banner: &ConnectionBanner) -> anyhow::Result<()>;
}

/// The exit a modal banner's action asks for.
#[must_use]
pub const fn banner_exit(action: BannerAction) -> Option<AttachExitReason> {
    match action {
        BannerAction::Quit => Some(AttachExitReason::Quit),
        BannerAction::DetachLocal => Some(AttachExitReason::Detached),
        BannerAction::Retry => None,
    }
}

/// What `input` does while `banner` shows: `Some` ends reconnecting.
fn answer(
    host: &mut (impl ReconnectHost + ?Sized),
    banner: &ConnectionBanner,
    input: ReconnectInput,
) -> anyhow::Result<Option<AttachExitReason>> {
    Ok(match input {
        ReconnectInput::Key(key) => banner.action_for(key).and_then(banner_exit),
        ReconnectInput::Closed(reason) => Some(reason),
        ReconnectInput::Repaint => {
            host.show(banner)?;
            None
        }
        ReconnectInput::Ignored => None,
    })
}

/// When the countdown on a banner due at `due` next changes (it shows whole
/// seconds left, rounded up).
fn next_countdown_change(now: Instant, due: Instant) -> Instant {
    let left = due.saturating_duration_since(now);
    let shown = left.as_millis().div_ceil(1000);
    let next = u64::try_from(shown.saturating_sub(1)).map_or(Duration::ZERO, Duration::from_secs);
    due.checked_sub(next).unwrap_or(due).max(now)
}

/// Reconnect a lost attachment in place: wait out the backoff, then run
/// `attempt_once` (connect and re-admit the attachment), until that
/// succeeds, the session is gone, the attempts run out, or the user leaves.
/// The banner says which attempt is next and when; keys it does not answer
/// are dropped.
///
/// # Errors
/// Returns a terminal failure (showing the banner, reading input).
pub async fn reconnect_in_place<H, T, A, Fut>(
    host: &mut H,
    policy: &ReconnectPolicy,
    server_stopped: bool,
    seed: u64,
    mut attempt_once: A,
) -> anyhow::Result<ReconnectEnd<T>>
where
    H: ReconnectHost + ?Sized,
    A: FnMut(AttachReconnectAttempt) -> Fut,
    Fut: Future<Output = Result<T, ReattachFailure>>,
{
    let lost_at = Instant::now();
    let mut attempt = 0_u32;
    // Attempts in a row that found the session gone.
    let mut gone_answers = 0_u32;
    loop {
        if policy
            .max_attempts
            .is_some_and(|max_attempts| attempt >= max_attempts)
        {
            tracing::warn!(attempts = attempt, "attach.reconnect.gave_up");
            return Ok(ReconnectEnd::Exit(AttachExitReason::ConnectionLost));
        }
        attempt = attempt.saturating_add(1);
        let backoff = policy.delay(attempt, seed);
        let due = Instant::now() + backoff;
        loop {
            let now = Instant::now();
            if now >= due {
                break;
            }
            let banner = ConnectionBanner::Reconnecting {
                attempt,
                next_in: Some(due - now),
            };
            host.show(&banner)?;
            tokio::select! {
                () = tokio::time::sleep_until(next_countdown_change(now, due)) => {}
                input = host.next_input() => {
                    if let Some(reason) = answer(host, &banner, input?)? {
                        return Ok(ReconnectEnd::Exit(reason));
                    }
                }
            }
        }

        let banner = ConnectionBanner::Reconnecting {
            attempt,
            next_in: None,
        };
        host.show(&banner)?;
        let reattached = attempt_once(AttachReconnectAttempt {
            number: attempt,
            outage: lost_at.elapsed(),
            backoff,
            server_stopped,
        });
        tokio::pin!(reattached);
        let outcome = loop {
            tokio::select! {
                outcome = &mut reattached => break outcome,
                input = host.next_input() => {
                    if let Some(reason) = answer(host, &banner, input?)? {
                        return Ok(ReconnectEnd::Exit(reason));
                    }
                }
            }
        };
        match outcome {
            Ok(reattached) => {
                tracing::info!(
                    attempt,
                    outage_ms = lost_at.elapsed().as_millis(),
                    "attach.reconnect.reattached"
                );
                return Ok(ReconnectEnd::Reattached(reattached));
            }
            Err(ReattachFailure::Gone) => {
                gone_answers = gone_answers.saturating_add(1);
                if gone_answers < policy.gone_confirmations {
                    tracing::info!(
                        attempt,
                        gone_answers,
                        "attach.reconnect.session_gone_unconfirmed"
                    );
                    continue;
                }
                tracing::info!(attempt, gone_answers, "attach.reconnect.session_gone");
                host.show(&ConnectionBanner::Gone)?;
                return Ok(ReconnectEnd::Exit(AttachExitReason::SessionGone));
            }
            Err(ReattachFailure::Retry(error))
                if error.downcast_ref::<ReconnectGiveUp>().is_some() =>
            {
                tracing::warn!(attempt, error = %format!("{error:#}"), "attach.reconnect.gave_up");
                return Ok(ReconnectEnd::Exit(AttachExitReason::ConnectionLost));
            }
            Err(ReattachFailure::Retry(error)) => {
                gone_answers = 0;
                tracing::info!(attempt, error = %format!("{error:#}"), "attach.reconnect.attempt_failed");
            }
        }
    }
}

#[cfg(test)]
#[path = "reconnect_tests.rs"]
mod tests;
