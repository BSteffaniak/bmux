//! Reconnecting in place: the banner it shows, its backoff, and how it ends.
use super::*;
use std::collections::VecDeque;

/// A terminal whose input arrives at scripted times; records every banner
/// shown, with when.
struct ScriptedHost {
    started: Instant,
    inputs: VecDeque<(Duration, ReconnectInput)>,
    shown: Vec<(Duration, ConnectionBanner)>,
}

impl ScriptedHost {
    fn new(inputs: impl IntoIterator<Item = (u64, ReconnectInput)>) -> Self {
        Self {
            started: Instant::now(),
            inputs: inputs
                .into_iter()
                .map(|(at, input)| (Duration::from_millis(at), input))
                .collect(),
            shown: Vec::new(),
        }
    }

    /// The attempts announced as running now, in order.
    fn attempts_started(&self) -> Vec<(u32, Duration)> {
        self.shown
            .iter()
            .filter_map(|(at, banner)| match banner {
                ConnectionBanner::Reconnecting {
                    attempt,
                    next_in: None,
                } => Some((*attempt, *at)),
                _ => None,
            })
            .collect()
    }
}

impl ReconnectHost for ScriptedHost {
    fn next_input(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<ReconnectInput>> + Send + '_>> {
        let started = self.started;
        Box::pin(async move {
            let Some((at, _)) = self.inputs.front() else {
                return std::future::pending().await;
            };
            tokio::time::sleep_until(started + *at).await;
            Ok(self.inputs.pop_front().expect("scripted input").1)
        })
    }

    fn show(&mut self, banner: &ConnectionBanner) -> anyhow::Result<()> {
        self.shown.push((self.started.elapsed(), banner.clone()));
        Ok(())
    }
}

const fn policy(max_attempts: Option<u32>) -> ReconnectPolicy {
    ReconnectPolicy {
        max_attempts,
        backoff: ReconnectBackoff::fixed(Duration::from_millis(100), Duration::from_millis(400)),
        first_delay: None,
    }
}

/// An attempt that fails `failures` times, then re-admits as `attempt`.
fn failing_then_ok(
    failures: u32,
) -> impl FnMut(AttachReconnectAttempt) -> std::future::Ready<Result<u32, ReattachFailure>> {
    move |attempt| {
        std::future::ready(if attempt.number <= failures {
            Err(ReattachFailure::Retry(anyhow::anyhow!(
                "connection refused"
            )))
        } else {
            Ok(attempt.number)
        })
    }
}

#[test]
fn attach_backoff_doubles_to_its_cap_within_its_jitter() {
    let nominal = [300, 600, 1_200, 2_400, 4_800, 5_000, 5_000];
    for seed in [0, 1, 42, u64::MAX] {
        for (index, nominal) in nominal.into_iter().enumerate() {
            let attempt = u32::try_from(index + 1).unwrap();
            let delay = ReconnectBackoff::ATTACH.delay(attempt, seed).as_millis();
            assert!(
                (nominal * 8 / 10..=nominal * 12 / 10).contains(&delay),
                "attempt {attempt}: {delay}ms is not within 20% of {nominal}ms"
            );
        }
    }
    // Different clients spread out.
    let spread: std::collections::BTreeSet<_> = (0..16)
        .map(|seed| ReconnectBackoff::ATTACH.delay(3, seed))
        .collect();
    assert!(spread.len() > 1);
}

#[test]
fn a_connector_tries_for_at_least_two_minutes() {
    for seed in [0, 1, 42, u64::MAX] {
        let budget: Duration = (1..=CONNECTOR_RECONNECT_MAX_ATTEMPTS)
            .map(|attempt| ReconnectBackoff::ATTACH.delay(attempt, seed))
            .sum();
        assert!(budget >= Duration::from_secs(120), "{budget:?}");
    }
}

#[test]
fn a_refused_device_gives_up_and_other_failures_retry() {
    let failure = |kind: std::io::ErrorKind| {
        bmux_client::ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(
            std::io::Error::new(kind, "desktop"),
        ))
    };
    assert!(
        connector_connect_error(failure(std::io::ErrorKind::PermissionDenied))
            .downcast_ref::<ReconnectGiveUp>()
            .is_some()
    );
    assert!(
        connector_connect_error(failure(std::io::ErrorKind::Other))
            .downcast_ref::<ReconnectGiveUp>()
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn attempts_wait_out_the_backoff_and_count_in_the_banner() {
    let mut host = ScriptedHost::new([]);
    let end = reconnect_in_place(&mut host, &policy(None), false, 0, failing_then_ok(3))
        .await
        .unwrap();
    assert!(matches!(end, ReconnectEnd::Reattached(4)));
    // 100, 200, 400, then the 400ms cap: each attempt after the one before.
    assert_eq!(
        host.attempts_started(),
        [
            (1, Duration::from_millis(100)),
            (2, Duration::from_millis(300)),
            (3, Duration::from_millis(700)),
            (4, Duration::from_millis(1_100)),
        ]
    );
    // While waiting, the banner says which attempt is next and when.
    assert_eq!(
        host.shown.first().map(|(_, banner)| banner.clone()),
        Some(ConnectionBanner::Reconnecting {
            attempt: 1,
            next_in: Some(Duration::from_millis(100)),
        })
    );
}

#[tokio::test(start_paused = true)]
async fn the_countdown_is_repainted_as_whole_seconds_pass() {
    let mut host = ScriptedHost::new([]);
    let policy = ReconnectPolicy {
        max_attempts: None,
        backoff: ReconnectBackoff::fixed(Duration::from_millis(2_500), Duration::from_secs(5)),
        first_delay: None,
    };
    let end = reconnect_in_place(&mut host, &policy, false, 0, failing_then_ok(0))
        .await
        .unwrap();
    assert!(matches!(end, ReconnectEnd::Reattached(1)));
    let texts: Vec<_> = host
        .shown
        .iter()
        .map(|(at, banner)| (at.as_millis(), banner.text()))
        .collect();
    assert_eq!(
        texts,
        [
            (
                0,
                "connection lost — reconnecting (attempt 1, next in 3s) · q quit · d detach-local"
                    .to_owned()
            ),
            (
                500,
                "connection lost — reconnecting (attempt 1, next in 2s) · q quit · d detach-local"
                    .to_owned()
            ),
            (
                1_500,
                "connection lost — reconnecting (attempt 1, next in 1s) · q quit · d detach-local"
                    .to_owned()
            ),
            (
                2_500,
                "connection lost — reconnecting (attempt 1…) · q quit · d detach-local".to_owned()
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn q_while_waiting_quits_and_other_keys_are_dropped() {
    let mut host = ScriptedHost::new([
        (50, ReconnectInput::Key('x')),
        (60, ReconnectInput::Repaint),
        (150, ReconnectInput::Key('q')),
    ]);
    let mut attempts = 0;
    let end = reconnect_in_place(&mut host, &policy(None), false, 0, |attempt| {
        attempts += 1;
        failing_then_ok(u32::MAX)(attempt)
    })
    .await
    .unwrap();
    assert!(matches!(end, ReconnectEnd::Exit(AttachExitReason::Quit)));
    assert_eq!(attempts, 1, "one attempt ran before q");
}

#[tokio::test(start_paused = true)]
async fn d_during_a_running_attempt_detaches_locally() {
    let mut host = ScriptedHost::new([(500, ReconnectInput::Key('d'))]);
    let end = reconnect_in_place(&mut host, &policy(None), false, 0, |_| {
        std::future::pending::<Result<(), ReattachFailure>>()
    })
    .await
    .unwrap();
    assert!(matches!(
        end,
        ReconnectEnd::Exit(AttachExitReason::Detached)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_closed_terminal_ends_reconnecting_its_way() {
    let mut host =
        ScriptedHost::new([(10, ReconnectInput::Closed(AttachExitReason::StreamClosed))]);
    let end = reconnect_in_place(&mut host, &policy(None), false, 0, failing_then_ok(0))
        .await
        .unwrap();
    assert!(matches!(
        end,
        ReconnectEnd::Exit(AttachExitReason::StreamClosed)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_session_gone_after_reconnecting_ends_with_the_gone_banner() {
    let mut host = ScriptedHost::new([]);
    let end = reconnect_in_place(&mut host, &policy(None), false, 0, |_| {
        std::future::ready(Err::<(), _>(ReattachFailure::Gone))
    })
    .await
    .unwrap();
    assert!(matches!(
        end,
        ReconnectEnd::Exit(AttachExitReason::SessionGone)
    ));
    assert_eq!(
        host.shown.last().map(|(_, banner)| banner),
        Some(&ConnectionBanner::Gone)
    );
}

#[tokio::test(start_paused = true)]
async fn a_bounded_policy_gives_up_as_connection_lost() {
    let mut host = ScriptedHost::new([]);
    let end = reconnect_in_place(
        &mut host,
        &policy(Some(2)),
        false,
        0,
        failing_then_ok(u32::MAX),
    )
    .await
    .unwrap();
    assert!(matches!(
        end,
        ReconnectEnd::Exit(AttachExitReason::ConnectionLost)
    ));
    assert_eq!(host.attempts_started().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_failure_retrying_cannot_fix_gives_up_at_once() {
    let mut host = ScriptedHost::new([]);
    let end = reconnect_in_place(&mut host, &policy(None), false, 0, |_| {
        std::future::ready(Err::<(), _>(ReattachFailure::Retry(
            anyhow::anyhow!("auth rejected").context(ReconnectGiveUp),
        )))
    })
    .await
    .unwrap();
    assert!(matches!(
        end,
        ReconnectEnd::Exit(AttachExitReason::ConnectionLost)
    ));
    assert_eq!(host.attempts_started().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_first_attempt_waits_as_long_as_the_other_side_asked() {
    let mut host = ScriptedHost::new([]);
    let policy = ReconnectPolicy {
        first_delay: Some(Duration::from_millis(1_234)),
        ..policy(None)
    };
    let mut seen = Vec::new();
    let end = reconnect_in_place(&mut host, &policy, true, 0, |attempt| {
        seen.push(attempt);
        failing_then_ok(1)(attempt)
    })
    .await
    .unwrap();
    assert!(matches!(end, ReconnectEnd::Reattached(2)));
    assert_eq!(
        host.attempts_started(),
        [
            (1, Duration::from_millis(1_234)),
            (2, Duration::from_millis(1_434)),
        ]
    );
    assert!(seen.iter().all(|attempt| attempt.server_stopped));
    assert_eq!(seen[1].backoff, Duration::from_millis(200));
    assert_eq!(seen[1].outage, Duration::from_millis(1_434));
}
