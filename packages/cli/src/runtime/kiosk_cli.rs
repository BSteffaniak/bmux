use super::{
    AttachExitReason, ConnectionContext, KernelClientFactory, SSH_RECONNECT_MAX_ATTEMPTS,
    connect_attach_target_with_kernel, connect_with_context, map_cli_client_error,
    reconnect_backoff_ms, run_session_attach_with_client,
};
use crate::connection::ConnectionPolicyScope;
use crate::pane_runtime_client::BmuxPaneRuntimeClientExt;
use anyhow::Result;
use std::{future::Future, pin::Pin, time::Duration};
pub(super) async fn attach_authorized(
    grant: bmux_connections_admin::kiosk::AuthorizedAttach,
) -> Result<u8> {
    let connection_context = ConnectionContext::new(None);
    let effective_target = grant.target;
    let (client, kernel_client_factory) = if let Some(target) = effective_target.as_deref() {
        connect_attach_target_with_kernel(target, "bmux-cli-kiosk-attach").await?
    } else {
        let client = connect_with_context(
            ConnectionPolicyScope::Normal,
            "bmux-cli-kiosk-attach",
            connection_context,
        )
        .await?;
        (client, None)
    };
    run_kiosk_attach_with_reconnect(
        client,
        kernel_client_factory,
        effective_target.as_deref(),
        grant.allow_detach,
        grant.session.as_deref(),
    )
    .await
}

async fn run_kiosk_attach_with_reconnect(
    client: bmux_client::BmuxClient,
    kernel_client_factory: Option<KernelClientFactory>,
    reconnect_target: Option<&str>,
    allow_detach: bool,
    session: Option<&str>,
) -> Result<u8> {
    let mut runner = LiveKioskAttachRunner {
        client: Some(client),
        kernel_client_factory,
    };
    run_kiosk_attach_with_reconnect_loop(&mut runner, reconnect_target, allow_detach, session).await
}

trait KioskAttachLoopDriver {
    fn set_attach_policy(
        &mut self,
        allow_detach: bool,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;

    fn run_attach<'a>(
        &'a mut self,
        session: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<super::AttachRunOutcome>> + Send + 'a>>;

    fn reconnect<'a>(
        &'a mut self,
        target: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    fn sleep_backoff(&mut self, backoff: Duration)
    -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

struct LiveKioskAttachRunner {
    client: Option<bmux_client::BmuxClient>,
    kernel_client_factory: Option<KernelClientFactory>,
}

impl KioskAttachLoopDriver for LiveKioskAttachRunner {
    fn set_attach_policy(
        &mut self,
        allow_detach: bool,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let client = self
                .client
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("kiosk attach client unavailable"))?;
            client
                .set_attach_policy(allow_detach)
                .await
                .map_err(map_cli_client_error)
        })
    }

    fn run_attach<'a>(
        &'a mut self,
        session: Option<&'a str>,
    ) -> Pin<Box<dyn Future<Output = Result<super::AttachRunOutcome>> + Send + 'a>> {
        Box::pin(async move {
            let client = self
                .client
                .take()
                .ok_or_else(|| anyhow::anyhow!("kiosk attach client unavailable"))?;
            let kernel_client_factory = self.kernel_client_factory.take();
            run_session_attach_with_client(client, session, None, false, kernel_client_factory)
                .await
        })
    }

    fn reconnect<'a>(
        &'a mut self,
        target: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let (new_client, new_kernel_factory) =
                connect_attach_target_with_kernel(target, "bmux-cli-kiosk-attach-reconnect")
                    .await?;
            self.client = Some(new_client);
            self.kernel_client_factory = new_kernel_factory;
            Ok(())
        })
    }

    fn sleep_backoff(
        &mut self,
        backoff: Duration,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(backoff).await;
        })
    }
}

async fn run_kiosk_attach_with_reconnect_loop(
    runner: &mut impl KioskAttachLoopDriver,
    reconnect_target: Option<&str>,
    allow_detach: bool,
    session: Option<&str>,
) -> Result<u8> {
    let mut attempt = 0usize;
    loop {
        runner.set_attach_policy(allow_detach).await?;
        let outcome = runner.run_attach(session).await?;
        if outcome.exit_reason != AttachExitReason::StreamClosed {
            return Ok(outcome.status_code);
        }

        let Some(target) = reconnect_target else {
            return Ok(outcome.status_code);
        };
        if attempt >= SSH_RECONNECT_MAX_ATTEMPTS {
            println!(
                "kiosk remote connection closed; giving up after {SSH_RECONNECT_MAX_ATTEMPTS} reconnect attempts"
            );
            return Ok(1);
        }

        attempt = attempt.saturating_add(1);
        let backoff = Duration::from_millis(reconnect_backoff_ms(attempt));
        println!(
            "kiosk remote connection closed; reconnecting to '{target}' (attempt {attempt}/{}) in {}ms...",
            SSH_RECONNECT_MAX_ATTEMPTS,
            backoff.as_millis()
        );
        runner.sleep_backoff(backoff).await;
        runner.reconnect(target).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::{KioskAttachLoopDriver, run_kiosk_attach_with_reconnect_loop};
    use crate::runtime::{AttachExitReason, AttachRunOutcome};
    use anyhow::Result;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::pin::Pin;
    use std::time::Duration;

    struct MockKioskAttachRunner {
        outcomes: VecDeque<AttachRunOutcome>,
        set_policy_calls: usize,
        reconnect_calls: usize,
        sleep_calls: usize,
    }

    impl MockKioskAttachRunner {
        fn with_outcomes(outcomes: Vec<AttachRunOutcome>) -> Self {
            Self {
                outcomes: outcomes.into(),
                set_policy_calls: 0,
                reconnect_calls: 0,
                sleep_calls: 0,
            }
        }
    }

    impl KioskAttachLoopDriver for MockKioskAttachRunner {
        fn set_attach_policy(
            &mut self,
            _allow_detach: bool,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
            self.set_policy_calls = self.set_policy_calls.saturating_add(1);
            Box::pin(async { Ok(()) })
        }

        fn run_attach<'a>(
            &'a mut self,
            _session: Option<&'a str>,
        ) -> Pin<Box<dyn Future<Output = Result<AttachRunOutcome>> + Send + 'a>> {
            let outcome = self
                .outcomes
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("test runner has no attach outcomes remaining"));
            Box::pin(async move { outcome })
        }

        fn reconnect<'a>(
            &'a mut self,
            _target: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            self.reconnect_calls = self.reconnect_calls.saturating_add(1);
            Box::pin(async { Ok(()) })
        }

        fn sleep_backoff(
            &mut self,
            _backoff: Duration,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.sleep_calls = self.sleep_calls.saturating_add(1);
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn reconnect_loop_reapplies_policy_after_stream_closed() {
        let mut runner = MockKioskAttachRunner::with_outcomes(vec![
            AttachRunOutcome {
                status_code: 1,
                exit_reason: AttachExitReason::StreamClosed,
                resume: None,
            },
            AttachRunOutcome {
                status_code: 0,
                exit_reason: AttachExitReason::Detached,
                resume: None,
            },
        ]);

        let status = run_kiosk_attach_with_reconnect_loop(
            &mut runner,
            Some("ssh://demo@host"),
            false,
            Some("session-a"),
        )
        .await
        .expect("loop should succeed");

        assert_eq!(status, 0);
        assert_eq!(runner.set_policy_calls, 2);
        assert_eq!(runner.reconnect_calls, 1);
        assert_eq!(runner.sleep_calls, 1);
    }

    #[tokio::test]
    async fn reconnect_loop_does_not_retry_without_target() {
        let mut runner = MockKioskAttachRunner::with_outcomes(vec![AttachRunOutcome {
            status_code: 7,
            exit_reason: AttachExitReason::StreamClosed,
            resume: None,
        }]);

        let status = run_kiosk_attach_with_reconnect_loop(&mut runner, None, true, None)
            .await
            .expect("loop should succeed");

        assert_eq!(status, 7);
        assert_eq!(runner.set_policy_calls, 1);
        assert_eq!(runner.reconnect_calls, 0);
        assert_eq!(runner.sleep_calls, 0);
    }
}
