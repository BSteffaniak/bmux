//! Managed-companion retirement driver. The execution plugin owns the idle
//! clock, blockers, and the admission fence; this module only requests its
//! generated BPDL operations and returns permission for graceful shutdown.
//!
//! No IPC client is opened: a diagnostic connection must not become an
//! attachment or otherwise keep an unused companion alive. The bootstrap owner
//! retains the server/run future and calls `request_shutdown` only on `Ok(())`.

use std::ffi::OsString;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::{
    ManagedIdleStatus, RuntimeReadiness, client,
};
use bmux_plugin::{ServiceCaller, ServiceCallerDispatchClient, block_on_typed_dispatch};

const DEFAULT_GRACE_SECONDS: u64 = 900;
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_MARKER_BYTES: usize = 64 * 1024;
const MAX_CONSECUTIVE_FAILURES: u8 = 3;

/// Immutable launch intent, captured before plugin activation. A grace variable
/// alone never converts an ordinary/slot/sandbox server to a managed companion.
/// The marker remains opaque here: identity ownership/validation belongs to the
/// Bterm selection lane, and the activated plugin must echo this exact JSON value.
#[derive(Clone)]
pub(super) struct ManagedIdlePolicy {
    marker: serde_json::Value,
    grace_seconds: u64,
}

impl ManagedIdlePolicy {
    pub(super) fn capture() -> Result<Option<Self>> {
        Self::from_environment(
            std::env::var_os("BMUX_MANAGED_TARGET_IDENTITY"),
            std::env::var_os("BMUX_MANAGED_IDLE_GRACE_SECONDS"),
        )
    }

    fn from_environment(marker: Option<OsString>, grace: Option<OsString>) -> Result<Option<Self>> {
        let Some(marker) = marker else {
            return Ok(None);
        };
        let marker = marker
            .into_string()
            .map_err(|_| anyhow::anyhow!("managed target launch marker is not UTF-8"))?;
        ensure!(
            !marker.is_empty() && marker.len() <= MAX_MARKER_BYTES,
            "managed target launch marker is empty or oversized"
        );
        // Do not echo malformed identity text into errors or logs.
        let marker: serde_json::Value = serde_json::from_str(&marker)
            .map_err(|_| anyhow::anyhow!("managed target launch marker is not valid JSON"))?;
        ensure!(
            marker.as_object().is_some_and(|object| !object.is_empty()),
            "managed target launch marker must be a nonempty identity object"
        );
        let grace_seconds = match grace {
            None => DEFAULT_GRACE_SECONDS,
            Some(grace) => {
                let grace = grace
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("managed idle grace is not UTF-8"))?;
                ensure!(
                    !grace.is_empty() && grace.bytes().all(|byte| byte.is_ascii_digit()),
                    "managed idle grace must be an unsigned integer number of seconds"
                );
                grace
                    .parse()
                    .context("managed idle grace exceeds u64 seconds")?
            }
        };
        Ok(Some(Self {
            marker,
            grace_seconds,
        }))
    }

    fn verify_readiness(&self, readiness: &RuntimeReadiness) -> Result<()> {
        let observed = readiness
            .managed_target_identity
            .as_deref()
            .context("activated execution plugin has no managed launch identity")?;
        ensure!(
            observed.len() <= MAX_MARKER_BYTES,
            "activated managed identity is oversized"
        );
        let observed: serde_json::Value = serde_json::from_str(observed)
            .map_err(|_| anyhow::anyhow!("activated managed identity is malformed"))?;
        ensure!(
            self.marker == observed,
            "activated execution plugin managed identity differs from launch selection"
        );
        ensure!(
            readiness.process_id == std::process::id(),
            "managed idle service is not owned by this process"
        );
        ensure!(
            readiness.helper_ready && !readiness.fenced,
            "managed idle policy requires ready, unfenced execution service"
        );
        Ok(())
    }

    fn verify_status(&self, instance: uuid::Uuid, status: &ManagedIdleStatus) -> Result<bool> {
        ensure!(
            status.instance == instance,
            "managed idle runtime instance changed"
        );
        ensure!(status.managed, "execution service lost managed idle policy");
        ensure!(
            status.grace_seconds == self.grace_seconds,
            "execution service managed idle policy changed"
        );
        if !status.fenced {
            return Ok(false);
        }
        ensure!(
            self.grace_seconds != 0,
            "disabled managed idle policy returned a shutdown fence"
        );
        ensure!(
            status.blocker.is_none(),
            "managed idle shutdown fence retains a work blocker"
        );
        ensure!(
            status.idle_for_seconds >= self.grace_seconds,
            "managed idle shutdown fence precedes selected grace"
        );
        Ok(true)
    }
}

/// Resolve only after the owning execution plugin atomically fences admission.
/// `None` never invokes the caller and stays pending for ordinary servers.
/// `Some` requires an in-process caller captured from the activated pane-runtime
/// lifecycle context, not a transport client or a remotely located provider.
///
/// An error is NOT shutdown permission. The bootstrap select branch must retain
/// the server and surface the error (or disable this watcher), never translate a
/// failed inspection into `request_shutdown`. Dropping this future is safe: it
/// never stops a server, removes ownership evidence, or terminates executions.
pub(super) async fn watch<C>(policy: Option<ManagedIdlePolicy>, caller: Option<&C>) -> Result<()>
where
    C: ServiceCaller + Clone + Send + Sync + 'static,
{
    let Some(policy) = policy else {
        return std::future::pending().await;
    };
    let caller = caller.context("managed idle watcher has no activated execution caller")?;
    ensure_local_provider()?;
    let readiness = dispatch(caller, |dispatch| {
        block_on_typed_dispatch(client::runtime_readiness(dispatch))
    })
    .await
    .context("inspect managed companion execution readiness")?;
    policy.verify_readiness(&readiness)?;
    let instance = readiness.instance;
    let grace_seconds = policy.grace_seconds;
    let configured = dispatch(caller, move |dispatch| {
        block_on_typed_dispatch(client::configure_managed_idle(
            dispatch,
            instance,
            grace_seconds,
        ))
    })
    .await
    .context("configure managed companion idle policy")?
    .map_err(anyhow::Error::msg)?;
    // Configuration is not the atomic retirement operation. It must never be
    // accepted as shutdown authority, even if a provider unexpectedly fences.
    ensure!(
        !configured.fenced,
        "managed idle configuration returned an unexpected fence"
    );
    policy.verify_status(instance, &configured)?;
    if policy.grace_seconds == 0 {
        return std::future::pending().await;
    }
    let mut consecutive_failures = 0_u8;
    loop {
        ensure_local_provider()?;
        // fence-managed-idle starts/advances the plugin-owned idle observation;
        // diagnostic managed-idle-status intentionally does not alter that clock.
        match dispatch(caller, move |dispatch| {
            block_on_typed_dispatch(client::fence_managed_idle(dispatch, instance))
        })
        .await
        {
            Ok(Ok(status)) => {
                consecutive_failures = 0;
                if policy.verify_status(instance, &status)? {
                    return Ok(());
                }
            }
            result => {
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    return match result {
                        Err(error) => Err(error)
                            .context("managed idle inspection repeatedly failed; runtime retained"),
                        Ok(Err(error)) => Err(anyhow::anyhow!(error))
                            .context("managed idle fence repeatedly refused; runtime retained"),
                        Ok(Ok(_)) => unreachable!("successful inspection handled above"),
                    };
                }
            }
        }
        // Delay rather than Interval avoids catch-up bursts after a busy server.
        // Client attachment/activity is never sampled as a synthetic keepalive.
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

// Generated helpers backed by ServiceCaller are synchronous, including nested
// kernel checks and artifact verification. Never poll them on the listener task.
// One awaited operation at a time; cancellation cannot undo an in-flight fence.
async fn dispatch<C, T>(
    caller: &C,
    operation: impl FnOnce(
        &mut ServiceCallerDispatchClient<'_, C>,
    ) -> std::result::Result<T, bmux_plugin_sdk::TypedServiceClientError>
    + Send
    + 'static,
) -> Result<T>
where
    C: ServiceCaller + Clone + Send + Sync + 'static,
    T: Send + 'static,
{
    let caller = caller.clone();
    tokio::task::spawn_blocking(move || {
        operation(&mut ServiceCallerDispatchClient::new(&caller)).map_err(anyhow::Error::from)
    })
    .await
    .context("managed idle service task failed")?
}

fn ensure_local_provider() -> Result<()> {
    ensure!(
        matches!(
            bmux_plugin::global_service_locations().get("bmux.pane_runtime"),
            Some(bmux_plugin::ServiceLocation::Local)
        ),
        "managed idle watcher requires the activated in-process execution provider"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(grace: Option<&str>) -> ManagedIdlePolicy {
        ManagedIdlePolicy::from_environment(
            Some(r#"{"selected":"exact-target"}"#.into()),
            grace.map(OsString::from),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn ordinary_servers_ignore_ambient_grace_without_managed_marker() {
        assert!(
            ManagedIdlePolicy::from_environment(None, Some("1".into()))
                .unwrap()
                .is_none()
        );
        assert!(
            ManagedIdlePolicy::from_environment(None, Some("invalid".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn explicit_policy_defaults_and_zero_disables() {
        assert_eq!(policy(None).grace_seconds, 900);
        assert_eq!(policy(Some("0")).grace_seconds, 0);
        assert_eq!(policy(Some("42")).grace_seconds, 42);
        for invalid in ["", "-1", "+1", " 1", "1 ", "0.1", "18446744073709551616"] {
            assert!(
                ManagedIdlePolicy::from_environment(
                    Some(r#"{"selected":"exact-target"}"#.into()),
                    Some(invalid.into()),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn malformed_markers_fail_closed_without_exposing_their_contents() {
        for marker in ["", "null", "[]", "{}", "private-invalid-marker"] {
            let error = ManagedIdlePolicy::from_environment(Some(marker.into()), None)
                .err()
                .unwrap();
            assert!(!error.to_string().contains("private-invalid-marker"));
        }
        assert!(
            ManagedIdlePolicy::from_environment(
                Some("x".repeat(MAX_MARKER_BYTES + 1).into()),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn activated_readiness_must_echo_selected_launch_and_current_process() {
        let selected = policy(None);
        let mut readiness = RuntimeReadiness {
            instance: uuid::Uuid::new_v4(),
            process_id: std::process::id(),
            executable_sha256: None,
            managed_target_identity: Some(r#"{ "selected": "exact-target" }"#.into()),
            runtime_dir: String::new(),
            config_dir: String::new(),
            data_dir: String::new(),
            state_dir: String::new(),
            helper_ready: true,
            helper_error: None,
            restart_blocker: None,
            fenced: false,
        };
        assert!(selected.verify_readiness(&readiness).is_ok());
        readiness.managed_target_identity = Some(r#"{"selected":"other-target"}"#.into());
        assert!(selected.verify_readiness(&readiness).is_err());
        readiness.managed_target_identity = Some(r#"{"selected":"exact-target"}"#.into());
        readiness.process_id = 0;
        assert!(selected.verify_readiness(&readiness).is_err());
        readiness.process_id = std::process::id();
        readiness.helper_ready = false;
        assert!(selected.verify_readiness(&readiness).is_err());
        readiness.helper_ready = true;
        readiness.fenced = true;
        assert!(selected.verify_readiness(&readiness).is_err());
        readiness.fenced = false;
        readiness.managed_target_identity = None;
        assert!(selected.verify_readiness(&readiness).is_err());
    }

    fn status(instance: uuid::Uuid) -> ManagedIdleStatus {
        ManagedIdleStatus {
            instance,
            managed: true,
            grace_seconds: 900,
            idle_for_seconds: 900,
            blocker: None,
            retained_authority: Vec::new(),
            fenced: true,
        }
    }

    #[test]
    fn only_exact_instance_policy_and_unblocked_elapsed_fence_permit_shutdown() {
        let policy = policy(None);
        let instance = uuid::Uuid::new_v4();
        let mut observed = status(instance);
        assert!(policy.verify_status(instance, &observed).unwrap());
        observed.fenced = false;
        observed.blocker = Some("detached shell".into());
        assert!(!policy.verify_status(instance, &observed).unwrap());
        observed.fenced = true;
        assert!(policy.verify_status(instance, &observed).is_err());
        observed.blocker = None;
        observed.idle_for_seconds = 899;
        assert!(policy.verify_status(instance, &observed).is_err());
        observed.idle_for_seconds = 900;
        observed.grace_seconds = 1;
        assert!(policy.verify_status(instance, &observed).is_err());
        observed.grace_seconds = 900;
        observed.managed = false;
        assert!(policy.verify_status(instance, &observed).is_err());
        observed.managed = true;
        assert!(
            policy
                .verify_status(uuid::Uuid::new_v4(), &observed)
                .is_err()
        );
        let disabled = ManagedIdlePolicy::from_environment(
            Some(r#"{"selected":"exact-target"}"#.into()),
            Some("0".into()),
        )
        .unwrap()
        .unwrap();
        observed.grace_seconds = 0;
        assert!(disabled.verify_status(instance, &observed).is_err());
    }

    #[derive(Clone)]
    struct NoCalls;
    impl ServiceCaller for NoCalls {
        fn call_service_raw(
            &self,
            _capability: &str,
            _kind: bmux_plugin_sdk::ServiceKind,
            _interface_id: &str,
            _operation: &str,
            _payload: Vec<u8>,
        ) -> bmux_plugin_sdk::Result<Vec<u8>> {
            panic!("ordinary server must not invoke managed idle services")
        }

        fn execute_kernel_request(
            &self,
            _request: bmux_ipc::Request,
        ) -> bmux_plugin_sdk::Result<bmux_ipc::ResponsePayload> {
            panic!("idle watcher must not open a kernel connection")
        }
    }

    #[tokio::test]
    async fn synchronous_dispatch_does_not_block_listener_polling() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let operation = dispatch(&NoCalls, move |_| {
            started_tx.send(()).unwrap();
            // A bounded wait makes a scheduling regression fail rather than hang.
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        });
        let listener = async {
            started_rx.await.unwrap();
            release_tx.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(operation, listener);
        result.unwrap();
        assert!(
            dispatch(&NoCalls, |_| {
                Err::<(), _>(
                    bmux_plugin_sdk::TypedDispatchClientError::transport(
                        "pane-runtime-readiness-v1",
                        "runtime-readiness",
                        "inspection failed",
                    )
                    .into(),
                )
            })
            .await
            .is_err(),
            "inspection failure must not become shutdown permission"
        );
    }

    #[tokio::test]
    async fn managed_server_requires_an_activated_caller() {
        assert!(watch::<NoCalls>(Some(policy(None)), None).await.is_err());
    }

    #[tokio::test]
    async fn ordinary_server_watcher_is_pending_and_makes_no_calls() {
        assert!(
            tokio::time::timeout(Duration::from_millis(1), watch(None, Some(&NoCalls)))
                .await
                .is_err()
        );
    }
}
