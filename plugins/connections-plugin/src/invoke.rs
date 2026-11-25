//! `connections-commands.invoke-service`: one typed service call on `local`
//! or a saved host, within one deadline. Calls are never replayed after
//! dispatch; only connection failures are retried.
use crate::connector::SavedHostConnector;
use bmux_client::BmuxClient;
use bmux_connections_plugin_api::connection_types::{
    ConnectionError, InvocationOptions, ServiceInvocation, ServiceInvokeKind,
};
use bmux_plugin_sdk::CancellationToken;
use std::time::Duration;
use tokio::time::Instant;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_ATTEMPTS: u32 = 10;
const MAX_BACKOFF_MS: u64 = 30_000;

pub fn normalized(options: Option<&InvocationOptions>) -> InvocationOptions {
    let options = options.cloned().unwrap_or(InvocationOptions {
        timeout_ms: DEFAULT_TIMEOUT_MS,
        max_attempts: 1,
        retry_backoff_ms: 0,
    });
    InvocationOptions {
        timeout_ms: options.timeout_ms,
        max_attempts: options.max_attempts.clamp(1, MAX_ATTEMPTS),
        retry_backoff_ms: options.retry_backoff_ms.min(MAX_BACKOFF_MS),
    }
}

fn timed_out(target: &str, phase: &str, timeout_ms: u64) -> ConnectionError {
    ConnectionError::TimedOut {
        target: target.to_owned(),
        phase: phase.to_owned(),
        timeout_ms,
    }
}

async fn open(
    paths: &bmux_config::ConfigPaths,
    target: &str,
) -> Result<BmuxClient, ConnectionError> {
    if target.is_empty() || target == "local" {
        return BmuxClient::connect_with_paths(paths, "bmux-connections-invoke")
            .await
            .map_err(|error| ConnectionError::ConnectionFailed {
                target: target.to_owned(),
                reason: error.to_string(),
            });
    }
    let connector = SavedHostConnector::new(paths.clone());
    if !bmux_client::TransportConnector::supports(&connector, target) {
        return Err(ConnectionError::TargetNotFound {
            target: target.to_owned(),
        });
    }
    connector
        .open(target, "bmux-connections-invoke")
        .await
        .map_err(|error| ConnectionError::ConnectionFailed {
            target: target.to_owned(),
            reason: format!("{error:#}"),
        })
}

pub async fn invoke(
    paths: &bmux_config::ConfigPaths,
    cancellation: &CancellationToken,
    invocation: ServiceInvocation,
) -> Result<Vec<u8>, ConnectionError> {
    let target = invocation.target.trim().to_owned();
    let options = normalized(invocation.options.as_ref());
    let timeout_ms = cancellation
        .deadline_ms
        .map_or(options.timeout_ms, |deadline| {
            deadline.min(options.timeout_ms)
        });
    if cancellation.is_cancelled() {
        return Err(ConnectionError::Cancelled { target });
    }
    if timeout_ms == 0 {
        return Err(timed_out(&target, "request", timeout_ms));
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_error = None;
    let mut client = None;
    for attempt in 1..=options.max_attempts {
        if cancellation.is_cancelled() {
            return Err(ConnectionError::Cancelled { target });
        }
        match tokio::time::timeout_at(deadline, open(paths, &target)).await {
            Err(_) => return Err(timed_out(&target, "connect", timeout_ms)),
            Ok(Ok(opened)) => {
                client = Some(opened);
                break;
            }
            Ok(Err(error @ ConnectionError::ConnectionFailed { .. })) => last_error = Some(error),
            Ok(Err(error)) => return Err(error),
        }
        if attempt < options.max_attempts {
            let backoff = Duration::from_millis(options.retry_backoff_ms);
            if Instant::now() + backoff >= deadline {
                return Err(timed_out(&target, "retry-backoff", timeout_ms));
            }
            tokio::time::sleep(backoff).await;
        }
    }
    let Some(mut client) = client else {
        let reason =
            last_error.map_or_else(|| "connection failed".to_owned(), |e| format!("{e:?}"));
        return Err(if options.max_attempts == 1 {
            ConnectionError::ConnectionFailed { target, reason }
        } else {
            ConnectionError::RetryExhausted {
                target,
                attempts: options.max_attempts,
                reason,
            }
        });
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timed_out(&target, "invoke", timeout_ms));
    }
    client.set_request_timeout(remaining);
    let kind = match invocation.kind {
        ServiceInvokeKind::InvokeQuery => bmux_ipc::InvokeServiceKind::Query,
        ServiceInvokeKind::InvokeCommand => bmux_ipc::InvokeServiceKind::Command,
    };
    match tokio::time::timeout(
        remaining,
        client.invoke_service_raw(
            &invocation.capability,
            kind,
            &invocation.interface_id,
            &invocation.operation,
            invocation.payload,
        ),
    )
    .await
    {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(error)) => Err(ConnectionError::ServiceFailed {
            target,
            reason: error.to_string(),
        }),
        Err(_) => Err(timed_out(&target, "invoke", timeout_ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_bounded() {
        let options = normalized(Some(&InvocationOptions {
            timeout_ms: 5,
            max_attempts: 99,
            retry_backoff_ms: 99_999,
        }));
        assert_eq!(options.max_attempts, MAX_ATTEMPTS);
        assert_eq!(options.retry_backoff_ms, MAX_BACKOFF_MS);
        assert_eq!(normalized(None).timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn unknown_targets_and_cancellation_fail_fast() {
        let root = tempfile::tempdir().unwrap();
        let paths = bmux_config::ConfigPaths::new(
            root.path().join("c"),
            root.path().join("r"),
            root.path().join("d"),
            root.path().join("s"),
        );
        let invocation = ServiceInvocation {
            target: "nowhere".into(),
            capability: "x".into(),
            kind: ServiceInvokeKind::InvokeQuery,
            interface_id: "x".into(),
            operation: "x".into(),
            payload: vec![],
            options: None,
        };
        assert_eq!(
            invoke(&paths, &CancellationToken::default(), invocation.clone()).await,
            Err(ConnectionError::TargetNotFound {
                target: "nowhere".into()
            })
        );
        let cancelled = CancellationToken::cancelled();
        assert!(matches!(
            invoke(&paths, &cancelled, invocation).await,
            Err(ConnectionError::Cancelled { .. })
        ));
    }
}
