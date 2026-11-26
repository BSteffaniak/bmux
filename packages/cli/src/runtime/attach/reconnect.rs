//! Resume an attachment whose connector-backed connection dropped.
//!
//! When the server stream of a connector-backed attach closes, or a call
//! fails after the connection recorded a transport failure (see
//! [`connection_loss_as_drop`]), the attach loop has already torn down its view and dropped every input it had not
//! written: nothing whose delivery is uncertain is kept, so nothing is
//! replayed. This loop reconnects through the same connector with capped
//! backoff and attaches again; the new attachment starts from the server's
//! snapshot of the same session.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use super::runtime::AttachRunOutcome;
use super::state::AttachExitReason;
use uuid::Uuid;

/// The session an attachment is on, published by the attach loop each time it
/// changes and kept after the attachment ends, however it ends: an attachment
/// that fails with an error reports no session of its own.
#[derive(Clone, Debug, Default)]
pub struct AttachedSessionCell(Arc<Mutex<Option<Uuid>>>);

impl AttachedSessionCell {
    pub fn set(&self, session: Uuid) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(session);
        }
    }

    #[must_use]
    pub fn get(&self) -> Option<Uuid> {
        self.0.lock().ok().and_then(|slot| *slot)
    }
}

/// The session to resume after a drop: the one the user was last on (the
/// tracked session, else the one the ended run reported), or the previous
/// target when neither is known. A follower keeps following instead.
#[must_use]
pub fn resume_session(
    previous: Option<String>,
    following: bool,
    last_attached: Option<Uuid>,
    reported: Option<Uuid>,
) -> Option<String> {
    if following {
        return previous;
    }
    last_attached
        .or(reported)
        .map(|session| session.to_string())
        .or(previous)
}

/// Reconnect attempts after one drop before giving up: with the backoff
/// below, at least two minutes of trying (longer while dials to an
/// unreachable host wait out their own timeouts), which outlasts a typical
/// network change.
pub const RECONNECT_MAX_ATTEMPTS: usize = 20;
const RECONNECT_BASE_BACKOFF: Duration = Duration::from_millis(250);
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(8);

/// Delay before reconnect attempt `attempt` (1-based): doubling from 250 ms,
/// capped at 8 s.
#[must_use]
pub fn reconnect_backoff(attempt: usize) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    RECONNECT_BASE_BACKOFF
        .checked_mul(2_u32.saturating_pow(exponent))
        .map_or(RECONNECT_MAX_BACKOFF, |delay| {
            delay.min(RECONNECT_MAX_BACKOFF)
        })
}

/// Discard keystrokes queued on the controlling terminal while the
/// attachment was down, so a resumed attachment never sends input typed
/// before it was ready.
pub fn discard_pending_terminal_input() {
    #[cfg(unix)]
    {
        // SAFETY: `tcflush` on this process's stdin descriptor only drops
        // queued, unread input; it fails harmlessly when stdin is not a tty.
        let _ = unsafe { libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH) };
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            FlushConsoleInputBuffer, GetStdHandle, STD_INPUT_HANDLE,
        };
        // SAFETY: flushing the standard input console buffer only drops
        // queued, unread input events; it fails harmlessly without a console.
        let _ = unsafe { FlushConsoleInputBuffer(GetStdHandle(STD_INPUT_HANDLE)) };
    }
}

/// Treat an attachment that failed because its connection was lost as a drop.
///
/// `transport_failure` is the connection's first recorded transport failure
/// (its stream failed or a request got no reply in time). Any error after such
/// a failure is a consequence of the lost connection, so the attachment ends
/// as [`AttachExitReason::StreamClosed`] and resumes; without one, the error
/// is a real attach failure and is returned unchanged.
///
/// # Errors
/// Returns `result`'s error when the connection recorded no failure.
pub fn connection_loss_as_drop(
    result: Result<AttachRunOutcome>,
    transport_failure: Option<String>,
) -> Result<AttachRunOutcome> {
    match (result, transport_failure) {
        (Err(error), Some(reason)) => {
            tracing::warn!(
                reason = %reason,
                error = %format!("{error:#}"),
                "attach.connection_lost"
            );
            Ok(AttachRunOutcome {
                status_code: 1,
                exit_reason: AttachExitReason::StreamClosed,
                resume: None,
                attached_session_id: None,
            })
        }
        (result, _) => result,
    }
}

/// Whether a connector refused this device itself: the saved-host connector
/// reports a revoked or unpaired device as an I/O `PermissionDenied`.
fn is_refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    })
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The effects a resumable attachment needs; tests substitute their own.
pub trait ResumableAttach: Send {
    /// The connection an attachment runs on.
    type Client: Send;

    /// Human label of the connection target, for messages.
    fn target_label(&self) -> &str;

    /// The session the user was last attached to, tracked across runs.
    fn last_attached_session(&self) -> Option<Uuid> {
        None
    }

    /// Run one attachment to completion on `client`.
    fn run<'a>(
        &'a mut self,
        client: Self::Client,
        session: Option<&'a str>,
    ) -> BoxFuture<'a, Result<AttachRunOutcome>>;

    /// Open (and prepare) a replacement client through the same connector.
    fn reconnect(&mut self) -> BoxFuture<'_, Result<Self::Client>>;

    fn sleep(&mut self, delay: Duration) -> BoxFuture<'_, ()>;

    /// Show a one-line status to the user (the terminal is restored here).
    fn notify(&mut self, message: &str);
}

/// Run `driver` until the attachment ends for any reason other than a lost
/// connection, reconnecting after each drop.
///
/// # Errors
/// Returns attach failures, or a resumable-message error after
/// [`RECONNECT_MAX_ATTEMPTS`] failed reconnects.
pub async fn run_resumable<D: ResumableAttach>(
    driver: &mut D,
    client: D::Client,
    session: Option<String>,
    following: bool,
) -> Result<AttachRunOutcome> {
    let mut client = client;
    let mut session = session;
    loop {
        let outcome = driver.run(client, session.as_deref()).await?;
        if !outcome.exit_reason.server_went_away() {
            return Ok(outcome);
        }
        // Resume the session the user was last on, even if it was chosen by
        // name or switched to after the attachment started.
        session = resume_session(
            session,
            following,
            driver.last_attached_session(),
            outcome.attached_session_id,
        );
        client = reconnect_with_backoff(driver, session.as_deref()).await?;
    }
}

async fn reconnect_with_backoff<D: ResumableAttach>(
    driver: &mut D,
    session: Option<&str>,
) -> Result<D::Client> {
    for attempt in 1..=RECONNECT_MAX_ATTEMPTS {
        let delay = reconnect_backoff(attempt);
        let message = format!(
            "bmux: connection to '{}' lost; reconnecting (attempt {attempt}/{RECONNECT_MAX_ATTEMPTS}) in {} ms…",
            driver.target_label(),
            delay.as_millis()
        );
        driver.notify(&message);
        driver.sleep(delay).await;
        match driver.reconnect().await {
            Ok(client) => return Ok(client),
            Err(error) => {
                tracing::warn!(attempt, error = %format!("{error:#}"), "attach.reconnect.failed");
                // The host refuses this device (revoked, never paired): no
                // retry can succeed, so the attachment ends now.
                if is_refused(&error) {
                    return Err(error);
                }
                let message = format!("bmux: reconnect failed: {error:#}");
                driver.notify(&message);
            }
        }
    }
    let resume = session.map_or_else(String::new, |session| format!(" attach {session}"));
    anyhow::bail!(
        "connection to '{target}' lost and {RECONNECT_MAX_ATTEMPTS} reconnect attempts failed; \
         input typed after the drop was not sent. Resume with `bmux --target {target}{resume}`",
        target = driver.target_label(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn outcome(exit_reason: AttachExitReason, session: Option<Uuid>) -> AttachRunOutcome {
        AttachRunOutcome {
            status_code: 0,
            exit_reason,
            resume: None,
            attached_session_id: session,
        }
    }

    /// Clients are numbered so tests can see which connection each run used.
    #[derive(Default)]
    struct Mock {
        outcomes: VecDeque<AttachRunOutcome>,
        reconnect_results: VecDeque<bool>,
        runs: Vec<(u32, Option<String>)>,
        tracked: Option<Uuid>,
        sleeps: Vec<Duration>,
        notices: Vec<String>,
        next_client: u32,
    }

    impl ResumableAttach for Mock {
        type Client = u32;

        fn target_label(&self) -> &'static str {
            "desktop"
        }

        fn last_attached_session(&self) -> Option<Uuid> {
            self.tracked
        }

        fn run<'a>(
            &'a mut self,
            client: u32,
            session: Option<&'a str>,
        ) -> BoxFuture<'a, Result<AttachRunOutcome>> {
            self.runs.push((client, session.map(str::to_owned)));
            let outcome = self.outcomes.pop_front().expect("scripted outcome");
            Box::pin(async move { Ok(outcome) })
        }

        fn reconnect(&mut self) -> BoxFuture<'_, Result<u32>> {
            let ok = self.reconnect_results.pop_front().unwrap_or(false);
            self.next_client += 1;
            let client = self.next_client;
            Box::pin(async move {
                if ok {
                    Ok(client)
                } else {
                    Err(anyhow::anyhow!("host offline"))
                }
            })
        }

        fn sleep(&mut self, delay: Duration) -> BoxFuture<'_, ()> {
            self.sleeps.push(delay);
            Box::pin(async {})
        }

        fn notify(&mut self, message: &str) {
            self.notices.push(message.to_owned());
        }
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(reconnect_backoff(1), Duration::from_millis(250));
        assert_eq!(reconnect_backoff(2), Duration::from_millis(500));
        assert_eq!(reconnect_backoff(6), Duration::from_secs(8));
        assert_eq!(reconnect_backoff(60), Duration::from_secs(8));
        let budget: Duration = (1..=RECONNECT_MAX_ATTEMPTS).map(reconnect_backoff).sum();
        assert!(budget > Duration::from_secs(90), "{budget:?}");
    }

    #[test]
    fn a_refused_device_is_not_retried() {
        let refused = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "this device's access was revoked on the host",
        ))
        .context("desktop");
        assert!(is_refused(&refused));
        assert!(!is_refused(&anyhow::anyhow!("the host did not answer")));
    }

    #[test]
    fn detach_and_quit_end_without_reconnecting() {
        for reason in [AttachExitReason::Detached, AttachExitReason::Quit] {
            let mut mock = Mock {
                outcomes: VecDeque::from([outcome(reason, None)]),
                ..Mock::default()
            };
            let result = block_on(run_resumable(&mut mock, 0, Some("main".into()), false)).unwrap();
            assert_eq!(result.exit_reason, reason);
            assert_eq!(mock.next_client, 0);
            assert_eq!(mock.notices, Vec::<String>::new());
        }
    }

    #[test]
    fn a_drop_resumes_the_attached_session_on_a_new_client() {
        let session = Uuid::new_v4();
        let mut mock = Mock {
            outcomes: VecDeque::from([
                outcome(AttachExitReason::StreamClosed, Some(session)),
                outcome(AttachExitReason::Detached, Some(session)),
            ]),
            reconnect_results: VecDeque::from([false, true]),
            ..Mock::default()
        };
        let result = block_on(run_resumable(&mut mock, 0, Some("main".into()), false)).unwrap();
        assert_eq!(result.exit_reason, AttachExitReason::Detached);
        assert_eq!(
            mock.runs,
            [(0, Some("main".into())), (2, Some(session.to_string()))]
        );
        assert_eq!(mock.sleeps, [reconnect_backoff(1), reconnect_backoff(2)]);
        assert!(mock.notices[0].contains("reconnecting (attempt 1/"));
        assert!(
            mock.notices
                .iter()
                .any(|notice| notice.contains("host offline"))
        );
    }

    #[test]
    fn errors_after_a_transport_failure_are_drops() {
        let lost = connection_loss_as_drop(
            Err(anyhow::anyhow!(
                "attach failed: request timed out after 30s"
            )),
            Some("request timed out after 30s".into()),
        )
        .unwrap();
        assert_eq!(lost.exit_reason, AttachExitReason::StreamClosed);
        assert_eq!(lost.attached_session_id, None);

        let error = connection_loss_as_drop(Err(anyhow::anyhow!("session not found")), None)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "session not found");

        let detached = connection_loss_as_drop(
            Ok(outcome(AttachExitReason::Detached, None)),
            Some("stream closed".into()),
        )
        .unwrap();
        assert_eq!(detached.exit_reason, AttachExitReason::Detached);
    }

    #[test]
    fn resume_targets_the_session_the_user_was_last_on() {
        let first = Uuid::new_v4();
        let switched = Uuid::new_v4();
        // Tracked switch wins over the run's report and the original name.
        assert_eq!(
            resume_session(Some("work".into()), false, Some(switched), Some(first)),
            Some(switched.to_string())
        );
        // An error-ended run reports nothing; the tracked session still counts.
        assert_eq!(
            resume_session(Some("work".into()), false, Some(switched), None),
            Some(switched.to_string())
        );
        assert_eq!(
            resume_session(Some("work".into()), false, None, Some(first)),
            Some(first.to_string())
        );
        assert_eq!(
            resume_session(Some("work".into()), false, None, None),
            Some("work".into())
        );
        assert_eq!(
            resume_session(None, true, Some(switched), Some(first)),
            None
        );
    }

    #[test]
    fn an_error_ended_run_resumes_the_tracked_session() {
        let switched = Uuid::new_v4();
        let mut mock = Mock {
            outcomes: VecDeque::from([
                // What connection_loss_as_drop yields: no session of its own.
                outcome(AttachExitReason::StreamClosed, None),
                outcome(AttachExitReason::Detached, None),
            ]),
            reconnect_results: VecDeque::from([true]),
            tracked: Some(switched),
            ..Mock::default()
        };
        block_on(run_resumable(&mut mock, 0, Some("main".into()), false)).unwrap();
        assert_eq!(
            mock.runs,
            [(0, Some("main".into())), (1, Some(switched.to_string()))]
        );
    }

    #[test]
    fn a_lost_connection_resumes_like_a_closed_stream() {
        let mut mock = Mock {
            outcomes: VecDeque::from([
                outcome(AttachExitReason::ConnectionLost, None),
                outcome(AttachExitReason::Detached, None),
            ]),
            reconnect_results: VecDeque::from([true]),
            ..Mock::default()
        };
        let result = block_on(run_resumable(&mut mock, 0, Some("main".into()), false)).unwrap();
        assert_eq!(result.exit_reason, AttachExitReason::Detached);
        assert_eq!(mock.runs.len(), 2);
    }

    #[test]
    fn following_keeps_the_follow_selection() {
        let mut mock = Mock {
            outcomes: VecDeque::from([
                outcome(AttachExitReason::StreamClosed, Some(Uuid::new_v4())),
                outcome(AttachExitReason::Quit, None),
            ]),
            reconnect_results: VecDeque::from([true]),
            ..Mock::default()
        };
        block_on(run_resumable(&mut mock, 0, None, true)).unwrap();
        assert_eq!(mock.runs, [(0, None), (1, None)]);
    }

    #[test]
    fn gives_up_with_a_resumable_message_after_the_bound() {
        let mut mock = Mock {
            outcomes: VecDeque::from([outcome(AttachExitReason::StreamClosed, None)]),
            ..Mock::default()
        };
        let error = block_on(run_resumable(&mut mock, 0, Some("work".into()), false))
            .unwrap_err()
            .to_string();
        assert_eq!(
            mock.next_client,
            u32::try_from(RECONNECT_MAX_ATTEMPTS).unwrap()
        );
        assert_eq!(mock.runs.len(), 1);
        assert!(error.contains("reconnect attempts failed"), "{error}");
        assert!(
            error.contains("bmux --target desktop attach work"),
            "{error}"
        );
    }
}
