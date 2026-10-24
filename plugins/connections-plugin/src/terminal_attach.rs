//! Single-terminal admitted attach adapter using only authorized execution services.
use anyhow::{Context, Result};
use bmux_attach_layout_protocol::{
    AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
};
use bmux_client::{
    AttachDeltaSequence, AttachDetachOutcome, AttachInputPayload, AttachProviderAck,
    AttachProviderAction, AttachProviderChange, AttachProviderDelta, AttachProviderEvent,
    AttachProviderInput, AttachProviderSnapshot, AttachProviderViewport, AttachResumeState,
    AttachSession, AttachSessionError, AttachSessionFuture, AttachStreamCursor, AttachStreamId,
    AttachStreamSnapshot, AttachViewRevision, BmuxClient,
};
use bmux_config::ConfigPaths;
use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
    TerminalExecutionRef, TerminalProgramOptions, client as execution,
};
use std::{collections::BTreeMap, fmt, time::Duration};

type PendingScreenRead = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(BmuxClient, bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionScreen)>> + Send>>;

pub struct RemoteTerminal {
    client: Option<BmuxClient>,
    paths: ConfigPaths,
    target: String,
    execution: TerminalExecutionRef,
    grid: bmux_terminal_grid::TerminalGridStream,
    sequence: u64,
    revision: u64,
    scene_dirty: bool,
    pending_read: Option<PendingScreenRead>,
    pending_recovery: bool,
    last_grid: Vec<u8>,
    read_client: Option<BmuxClient>,
    detached: bool,
    read_failures: u32,
    controls: bmux_client::AttachControlValidator,
    continuity: bmux_client::AttachContinuityValidator,
}
#[cfg(test)]
mod tests {
    use super::*;
    use bmux_connections_plugin_api::connection_types::ConnectionError;
    use bmux_plugin_sdk::{TypedDispatchClientError, TypedServiceClientError};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn terminal(pending_read: PendingScreenRead) -> RemoteTerminal {
        RemoteTerminal {
            client: None,
            paths: ConfigPaths::new(
                "/unused/config".into(),
                "/unused/runtime".into(),
                "/unused/data".into(),
                "/unused/state".into(),
            ),
            target: "unused".into(),
            execution: TerminalExecutionRef {
                session_id: uuid::Uuid::new_v4(),
                pane_id: uuid::Uuid::new_v4(),
                execution_token: uuid::Uuid::new_v4(),
            },
            grid: bmux_terminal_grid::TerminalGridStream::new(
                80,
                24,
                bmux_terminal_grid::GridLimits::default(),
            )
            .unwrap(),
            sequence: 0,
            revision: 1,
            scene_dirty: false,
            pending_read: Some(pending_read),
            pending_recovery: true,
            last_grid: vec![],
            read_client: None,
            detached: false,
            read_failures: 0,
            controls: bmux_client::AttachControlValidator::default(),
            continuity: bmux_client::AttachContinuityValidator::default(),
        }
    }

    #[test]
    fn durable_execution_keys_survive_removed_runtime_and_alias_rename() {
        let root = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        std::fs::create_dir_all(&paths.runtime_dir).unwrap();
        std::fs::create_dir_all(&paths.state_dir).unwrap();
        let endpoint = iroh::SecretKey::generate().public().to_string();
        let route = format!("device://{endpoint}?addr=127.0.0.1:1234");
        let reference = terminal(Box::pin(std::future::pending())).execution;
        let encoded = serde_json::to_string(&reference).unwrap();
        // Reproduce the old runtime-scoped file, then remove the live runtime.
        crate::remote_preferences::update(&paths, |p| {
            p.aliases.insert("desktop".into(), route.clone());
            p.terminals.insert("desktop/shell".into(), encoded.clone());
            Ok(())
        })
        .unwrap();
        std::fs::remove_dir(&paths.runtime_dir).unwrap();
        let config = bmux_config::BmuxConfig::default();
        let key = terminal_key(&endpoint, "shell").unwrap();
        crate::remote_preferences::client_update(&paths.state_dir, "default", |p| {
            migrate_terminal_keys(p, &config, &endpoint)?;
            assert_eq!(p.terminals.get(&key), Some(&encoded));
            assert!(!p.terminals.contains_key("desktop/shell"));
            Ok(())
        })
        .unwrap();
        crate::remote_preferences::rename_host(&paths.state_dir, "default", "desktop", "renamed")
            .unwrap();
        crate::remote_preferences::client_update(&paths.state_dir, "default", |p| {
            migrate_terminal_keys(p, &config, &endpoint)?;
            assert_eq!(p.terminals.get(&key), Some(&encoded));
            assert_eq!(p.aliases.get("renamed"), Some(&route));
            assert!(!p.aliases.contains_key("desktop"));
            Ok(())
        })
        .unwrap();
        assert!(!paths.runtime_dir.exists());
        forget_terminals(&paths, &config, &endpoint).unwrap();
        crate::remote_preferences::forget_host(&paths.state_dir, "default", "renamed").unwrap();
        crate::remote_preferences::client_update(&paths.state_dir, "default", |p| {
            assert!(p.terminals.is_empty());
            Ok(())
        })
        .unwrap();
        assert!(!paths.runtime_dir.exists());
    }

    #[test]
    fn terminal_key_migration_conflict_rolls_back_and_hosts_do_not_collide() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let host = iroh::SecretKey::generate().public().to_string();
        let other = iroh::SecretKey::generate().public().to_string();
        assert_ne!(
            terminal_key(&host, "shell").unwrap(),
            terminal_key(&other, "shell").unwrap()
        );
        let key = terminal_key(&host, "shell").unwrap();
        let first =
            serde_json::to_string(&terminal(Box::pin(std::future::pending())).execution).unwrap();
        let second =
            serde_json::to_string(&terminal(Box::pin(std::future::pending())).execution).unwrap();
        crate::remote_preferences::client_update(&state, "default", |p| {
            p.aliases.insert(
                "host".into(),
                format!("device://{host}?addr=127.0.0.1:1234"),
            );
            p.terminals.insert("host/shell".into(), first.clone());
            p.terminals.insert(key.clone(), second.clone());
            Ok(())
        })
        .unwrap();
        assert!(
            crate::remote_preferences::client_update(&state, "default", |p| {
                migrate_terminal_keys(p, &bmux_config::BmuxConfig::default(), &host)
            })
            .is_err()
        );
        crate::remote_preferences::client_update(&state, "default", |p| {
            assert_eq!(p.terminals.get("host/shell"), Some(&first));
            assert_eq!(p.terminals.get(&key), Some(&second));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn retries_only_transport_failures_not_denial_or_identity_mismatch() {
        assert!(retryable(
            &AttachConnectError(ConnectionError::ConnectionFailed {
                target: "t".into(),
                reason: "offline".into()
            })
            .into()
        ));
        for error in [
            ConnectionError::TrustFailed {
                target: "t".into(),
                reason: "revoked".into(),
            },
            ConnectionError::AuthenticationFailed {
                target: "t".into(),
                reason: "identity mismatch".into(),
            },
            ConnectionError::InvalidTarget {
                target: "t".into(),
                reason: "invalid".into(),
            },
        ] {
            assert!(!retryable(&AttachConnectError(error).into()));
        }
        assert!(retryable(&anyhow::Error::new(
            TypedServiceClientError::Dispatch(TypedDispatchClientError::transport(
                "i", "screen", "closed"
            ))
        )));
        assert!(!retryable(&anyhow::Error::new(
            TypedServiceClientError::Dispatch(TypedDispatchClientError::server(
                "i", "screen", "denied"
            ))
        )));
        assert!(!retryable(&anyhow::anyhow!("execution identity mismatch")));
        assert_eq!(
            (0..=3).map(retry_delay).collect::<Vec<_>>(),
            [100, 200, 400, 800].map(Duration::from_millis)
        );
        assert_eq!(retry_delay(u32::MAX), Duration::from_millis(800));
    }

    #[tokio::test]
    async fn cancelled_poll_keeps_owned_read_and_denial_is_terminal() {
        let starts = Arc::new(AtomicUsize::new(0));
        let started = starts.clone();
        let mut terminal = terminal(Box::pin(async move {
            started.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await;
            Err(anyhow::anyhow!("revoked"))
        }));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), terminal.next_event())
                .await
                .is_err()
        );
        assert!(terminal.pending_read.is_some());
        assert!(terminal.next_event().await.is_err());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(terminal.detached);
        assert!(terminal.client.is_none());
        assert!(matches!(
            terminal.next_event().await.unwrap(),
            AttachProviderEvent::Detached
        ));
    }

    #[tokio::test]
    async fn layout_action_is_rejected_explicitly_without_input_or_credentials() {
        let mut terminal = terminal(Box::pin(std::future::pending()));
        let ack = terminal
            .execute_action(AttachProviderAction {
                command_id: "layout-1".into(),
                action: "focus".into(),
                arguments: vec!["left".into()],
            })
            .await
            .unwrap();
        assert!(!ack.accepted);
        assert_eq!(ack.command_id.as_deref(), Some("layout-1"));
        assert!(ack.message.unwrap().contains("local layout keybinding"));
        // Rejection must not be disguised as a successful input or replay.
        assert!(terminal.client.is_none());
        assert!(
            terminal
                .execute_action(AttachProviderAction {
                    command_id: "layout-1".into(),
                    action: "focus".into(),
                    arguments: vec![],
                })
                .await
                .is_err()
        );
        let ack = terminal
            .execute_action(AttachProviderAction {
                command_id: "unknown-2".into(),
                action: "secret-bearing-unknown-action".into(),
                arguments: vec!["secret".into()],
            })
            .await
            .unwrap();
        assert!(!ack.accepted);
        assert!(!ack.message.unwrap().contains("secret"));
    }

    #[tokio::test]
    async fn retry_budget_exhaustion_does_not_dial_or_enable_controls() {
        let mut terminal = terminal(Box::pin(async {
            Err(AttachConnectError(ConnectionError::ConnectionFailed {
                target: "t".into(),
                reason: "offline".into(),
            })
            .into())
        }));
        terminal.read_failures = MAX_READ_RETRIES;
        assert!(terminal.next_event().await.is_err());
        assert!(terminal.detached);
        assert!(terminal.client.is_none());
        assert!(terminal.pending_read.is_none());
    }

    #[tokio::test]
    async fn detach_drops_pending_request_and_connections() {
        let mut terminal = terminal(Box::pin(std::future::pending()));
        terminal.detach().await.unwrap();
        assert!(terminal.pending_read.is_none());
        assert!(terminal.client.is_none());
        assert!(terminal.read_client.is_none());
        assert!(matches!(
            terminal.next_event().await.unwrap(),
            AttachProviderEvent::Detached
        ));
    }

    #[test]
    fn execution_token_mismatch_is_not_resumable() {
        let reference = terminal(Box::pin(std::future::pending())).execution;
        assert!(same_execution(&reference, &reference));
        let mut replacement = reference.clone();
        replacement.execution_token = uuid::Uuid::new_v4();
        assert!(!same_execution(&reference, &replacement));
        replacement = reference.clone();
        replacement.pane_id = uuid::Uuid::new_v4();
        assert!(!same_execution(&reference, &replacement));
    }
}

impl fmt::Debug for RemoteTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteTerminal { credentials: [redacted] }")
    }
}
fn failed(error: impl fmt::Display) -> AttachSessionError {
    AttachSessionError::Provider {
        reason: error.to_string(),
    }
}
fn operation<T, E: fmt::Debug>(result: std::result::Result<T, E>) -> Result<T> {
    result.map_err(|e| anyhow::anyhow!("remote operation denied or failed: {e:?}"))
}

const MAX_READ_RETRIES: u32 = 3;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct AttachConnectError(bmux_connections_plugin_api::connection_types::ConnectionError);
impl fmt::Display for AttachConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "remote attach connection failed: {:?}", self.0)
    }
}
impl std::error::Error for AttachConnectError {}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_millis(100 * (1_u64 << failures.min(MAX_READ_RETRIES)))
}

fn retryable(error: &anyhow::Error) -> bool {
    use bmux_connections_plugin_api::connection_types::ConnectionError;
    use bmux_plugin_sdk::{TypedDispatchClientError, TypedServiceClientError};
    error.chain().any(|cause| {
        cause.is::<tokio::time::error::Elapsed>()
            || matches!(
                cause.downcast_ref::<AttachConnectError>(),
                Some(AttachConnectError(
                    ConnectionError::ConnectionFailed { .. } | ConnectionError::TimedOut { .. }
                ))
            )
            || matches!(
                cause.downcast_ref::<TypedServiceClientError>(),
                Some(TypedServiceClientError::Dispatch(
                    TypedDispatchClientError::Transport { .. }
                ))
            )
    })
}

fn validate_screen(
    reference: &TerminalExecutionRef,
    screen: &bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::TerminalExecutionScreen,
) -> Result<()> {
    anyhow::ensure!(
        same_execution(reference, &screen.execution),
        "remote screen execution identity mismatch"
    );
    Ok(())
}

fn same_execution(a: &TerminalExecutionRef, b: &TerminalExecutionRef) -> bool {
    a.session_id == b.session_id && a.pane_id == b.pane_id && a.execution_token == b.execution_token
}

async fn validate_control(client: &mut BmuxClient, reference: &TerminalExecutionRef) -> Result<()> {
    use bmux_pane_runtime_plugin_api::pane_runtime_execution_v4::{
        TerminalLaunchPhase, TerminalSessionEnd,
    };
    let state = operation(execution::reconcile(client, reference.clone()).await?)?;
    anyhow::ensure!(
        same_execution(reference, &state.launch.reservation.execution),
        "remote control execution identity mismatch"
    );
    anyhow::ensure!(
        !state.recovery_required
            && !state.terminal_session_ended
            && matches!(state.session_end, TerminalSessionEnd::Active)
            && matches!(state.launch.phase, TerminalLaunchPhase::Running),
        "remote execution requires recovery; input remains disabled"
    );
    Ok(())
}

fn terminal_key(endpoint: &str, name: &str) -> Result<String> {
    crate::remote_preferences::name_valid(name)?;
    let endpoint: iroh::EndpointId = endpoint.parse()?;
    Ok(format!("endpoint:{endpoint}/{name}"))
}

// Upgrade alias/route keys only when their configured route identifies this
// authenticated host. Conflicts abort the transaction rather than losing tokens.
fn migrate_terminal_keys(
    preferences: &mut crate::remote_preferences::ClientPreferences,
    config: &bmux_config::BmuxConfig,
    endpoint: &str,
) -> Result<()> {
    let mut moves = Vec::new();
    for (key, value) in &preferences.terminals {
        if key.starts_with("endpoint:") {
            continue;
        }
        let Some((owner, name)) = key.rsplit_once('/') else {
            continue;
        };
        let route = preferences.aliases.get(owner).map_or(owner, String::as_str);
        let Ok(crate::target::ResolvedTarget::Iroh(target)) =
            crate::target::resolve_internal(config, route)
        else {
            continue;
        };
        if target.endpoint_id.parse::<iroh::EndpointId>()?.to_string() != endpoint {
            continue;
        }
        let reference: TerminalExecutionRef = serde_json::from_str(value)?;
        moves.push((
            key.clone(),
            terminal_key(endpoint, name)?,
            serde_json::to_string(&reference)?,
        ));
    }
    for (old, key, value) in moves {
        if let Some(existing) = preferences.terminals.get(&key) {
            let existing: TerminalExecutionRef = serde_json::from_str(existing)?;
            let migrated: TerminalExecutionRef = serde_json::from_str(&value)?;
            anyhow::ensure!(
                same_execution(&existing, &migrated),
                "conflicting saved remote execution credentials"
            );
        } else {
            preferences.terminals.insert(key, value);
        }
        preferences.terminals.remove(&old);
    }
    Ok(())
}

/// Forget private execution credentials for a host, including migrated legacy
/// alias/route records. Call before deleting its aliases. Does not stop remote PTYs.
pub(crate) fn forget_terminals(
    paths: &ConfigPaths,
    config: &bmux_config::BmuxConfig,
    endpoint: &str,
) -> Result<()> {
    let endpoint = endpoint.parse::<iroh::EndpointId>()?.to_string();
    crate::remote_preferences::client_update(&paths.state_dir, "default", |p| {
        migrate_terminal_keys(p, config, &endpoint)?;
        let prefix = format!("endpoint:{endpoint}/");
        p.terminals.retain(|key, _| !key.starts_with(&prefix));
        Ok(())
    })
}

impl RemoteTerminal {
    /// Open or resume one named terminal. Execution credentials stay in private local storage.
    ///
    /// # Errors
    /// Fails on missing pairing, resource grants, unavailable execution or incompatible service.
    pub async fn open(paths: ConfigPaths, target: String, name: String) -> Result<Self> {
        crate::remote_preferences::name_valid(&name)?;
        let (mut client, endpoint, route) =
            crate::transport::connect_paired_attach_host(&paths, &target)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let config = bmux_config::BmuxConfig::load_isolated_with_paths_and_overrides(
            &paths,
            &bmux_config::ConfigLoadOverrides::default(),
        )?;
        let key = terminal_key(&endpoint, &name)?;
        let saved = crate::remote_preferences::client_update(&paths.state_dir, "default", |p| {
            migrate_terminal_keys(p, &config, &endpoint)?;
            // A legacy record may have used a raw display alias that is not in
            // preferences (for example a configured connection name).
            let legacy = format!("{target}/{name}");
            if let Some(value) = p.terminals.remove(&legacy) {
                let reference: TerminalExecutionRef = serde_json::from_str(&value)?;
                if let Some(existing) = p.terminals.get(&key) {
                    anyhow::ensure!(
                        same_execution(&reference, &serde_json::from_str(existing)?),
                        "conflicting saved remote execution credentials"
                    );
                } else {
                    p.terminals.insert(key.clone(), value);
                }
            }
            Ok(p.terminals.get(&key).cloned())
        })?;
        let execution = if let Some(value) = saved {
            serde_json::from_str(&value)?
        } else {
            // Persist the reservation BEFORE the non-idempotent launch. Reopening
            // reconciles this exact execution; it must never silently launch another.
            let reservation = operation(
                execution::reserve(
                    &mut client,
                    uuid::Uuid::new_v4(),
                    TerminalProgramOptions {
                        program: "/bin/sh".into(),
                        args: vec![],
                        cwd: None,
                        env: BTreeMap::new(),
                        cols: 80,
                        rows: 24,
                    },
                )
                .await?,
            )?;
            let reference = reservation.execution.clone();
            crate::remote_preferences::client_update(&paths.state_dir, "default", |p| {
                anyhow::ensure!(
                    !p.terminals.contains_key(&key),
                    "remote terminal was concurrently reserved; reopen it"
                );
                p.terminals
                    .insert(key.clone(), serde_json::to_string(&reference)?);
                Ok(())
            })?;
            operation(execution::launch(&mut client, reservation).await?)?;
            reference
        };
        validate_control(&mut client, &execution).await?;
        let screen = operation(execution::screen(&mut client, execution.clone()).await?)?;
        validate_screen(&execution, &screen)?;
        let snapshot = serde_json::from_slice(&screen.encoded_grid)?;
        let grid = bmux_terminal_grid::TerminalGridStream::from_snapshot(
            &snapshot,
            bmux_terminal_grid::GridLimits::default(),
        )?;
        Ok(Self {
            client: Some(client),
            paths,
            target: route,
            execution,
            grid,
            sequence: 0,
            revision: 1,
            scene_dirty: false,
            pending_read: None,
            pending_recovery: false,
            last_grid: screen.encoded_grid,
            read_client: None,
            detached: false,
            read_failures: 0,
            controls: bmux_client::AttachControlValidator::default(),
            continuity: bmux_client::AttachContinuityValidator::default(),
        })
    }
    fn cursor(&self) -> Result<AttachStreamCursor, AttachSessionError> {
        Ok(AttachStreamCursor {
            stream_id: AttachStreamId::new(self.execution.pane_id.to_string())?,
            surface_id: self.execution.pane_id,
            generation: 1,
            offset: self.sequence,
        })
    }
    fn scene(&self) -> AttachScene {
        let snapshot = self.grid.grid().snapshot(0, self.grid.grid().height());
        let rect = AttachRect {
            x: 0,
            y: 0,
            w: snapshot.width,
            h: snapshot.height,
        };
        AttachScene {
            session_id: self.execution.session_id,
            focus: AttachFocusTarget::Pane {
                pane_id: self.execution.pane_id,
            },
            surfaces: vec![AttachSurface {
                id: self.execution.pane_id,
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect,
                content_rect: rect,
                interactive_regions: vec![],
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(self.execution.pane_id),
            }],
        }
    }
    fn resume(&self) -> Result<AttachResumeState, AttachSessionError> {
        Ok(AttachResumeState {
            view_revision: AttachViewRevision(self.revision),
            event_sequence: AttachDeltaSequence(self.sequence),
            streams: vec![self.cursor()?],
            provider_token: vec![],
        })
    }
    const fn ack() -> AttachProviderAck {
        AttachProviderAck {
            command_id: None,
            accepted: true,
            message: None,
        }
    }
    /// Prepare one owned read/recovery request. Keeping the future on the session
    /// preserves it across `next_event` cancellation; controls are reconciled only
    /// on a newly authenticated connection, never replayed by this read path.
    fn prepare_screen_read(&mut self) {
        // A cancelled control request leaves its outcome uncertain. Any earlier
        // screen read must not enable controls without reconciliation.
        if self.client.is_none() && !self.pending_recovery {
            self.pending_read = None;
            self.read_client = None;
        }
        if self.pending_read.is_some() {
            return;
        }
        let read_client = self.read_client.take();
        let paths = self.paths.clone();
        let target = self.target.clone();
        let reference = self.execution.clone();
        let reconnect_control = self.client.is_none();
        self.pending_recovery = reconnect_control;
        let failures = self.read_failures;
        self.pending_read = Some(Box::pin(async move {
            tokio::time::sleep(retry_delay(failures)).await;
            tokio::time::timeout(REQUEST_TIMEOUT, async move {
                let mut client = if let Some(client) = read_client {
                    client
                } else {
                    crate::transport::connect_paired_for_attach(&paths, &target)
                        .await
                        .map_err(AttachConnectError)?
                };
                if reconnect_control {
                    validate_control(&mut client, &reference).await?;
                }
                let screen = operation(execution::screen(&mut client, reference.clone()).await?)?;
                validate_screen(&reference, &screen)?;
                Ok((client, screen))
            })
            .await?
        }));
    }

    async fn refresh(&mut self) -> Result<(), AttachSessionError> {
        self.pending_read = None;
        let mut client = self
            .client
            .take()
            .ok_or_else(|| failed("control reconnect required"))?;
        let screen = operation(
            execution::screen(&mut client, self.execution.clone())
                .await
                .map_err(failed)?,
        )
        .map_err(failed)?;
        validate_screen(&self.execution, &screen).map_err(failed)?;
        self.client = Some(client);
        let snapshot = serde_json::from_slice(&screen.encoded_grid).map_err(failed)?;
        self.grid = bmux_terminal_grid::TerminalGridStream::from_snapshot(
            &snapshot,
            bmux_terminal_grid::GridLimits::default(),
        )
        .map_err(failed)?;
        Ok(())
    }
}
impl AttachSession for RemoteTerminal {
    fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
        Box::pin(async move {
            self.refresh().await?;
            let snapshot = AttachProviderSnapshot {
                view_revision: AttachViewRevision(self.revision),
                event_sequence: AttachDeltaSequence(self.sequence),
                scene: self.scene(),
                streams: vec![AttachStreamSnapshot {
                    cursor: self.cursor()?,
                    snapshot: bmux_terminal_grid::full_screen_repaint_bytes(self.grid.grid()),
                }],
                resume: self.resume()?,
            };
            self.continuity.apply_snapshot(&snapshot)?;
            Ok(snapshot)
        })
    }
    fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent> {
        Box::pin(async move {
            if self.detached {
                return Ok(AttachProviderEvent::Detached);
            }
            loop {
                self.prepare_screen_read();
                // Keep the whole owned request alive across select cancellation. A read
                // cannot leave an unconsumed response on the input/control connection.
                let screen = self.pending_read.as_mut().expect("pending read").await;
                self.pending_read = None;
                let (client, screen) = match screen {
                    Ok(value) => value,
                    Err(error) => {
                        // Neither a failed nor a cancelled request may reuse a
                        // connection with an uncertain unread response.
                        self.client = None;
                        self.read_client = None;
                        if retryable(&error) && self.read_failures < MAX_READ_RETRIES {
                            self.read_failures += 1;
                            continue;
                        }
                        self.detached = true;
                        return Err(failed(error));
                    }
                };
                self.read_failures = 0;
                let recovering = self.client.is_none();
                if recovering {
                    self.scene_dirty = true;
                }
                if screen.encoded_grid == self.last_grid && !self.scene_dirty {
                    self.read_client = Some(client);
                    continue;
                }
                let decoded = (|| -> Result<_> {
                    let snapshot = serde_json::from_slice(&screen.encoded_grid)?;
                    Ok(bmux_terminal_grid::TerminalGridStream::from_snapshot(
                        &snapshot,
                        bmux_terminal_grid::GridLimits::default(),
                    )?)
                })();
                let grid = match decoded {
                    Ok(grid) => grid,
                    Err(error) => {
                        self.client = None;
                        self.read_client = None;
                        self.detached = true;
                        return Err(failed(error));
                    }
                };
                if recovering {
                    // Restore controls only after authenticated reconciliation
                    // AND complete validation of the repaired screen.
                    self.client = Some(client);
                } else {
                    self.read_client = Some(client);
                }
                if grid.grid().width() != self.grid.grid().width()
                    || grid.grid().height() != self.grid.grid().height()
                {
                    self.scene_dirty = true;
                }
                self.grid = grid;
                self.last_grid = screen.encoded_grid;
                break;
            }
            let base_revision = self.revision;
            let mut changes = Vec::new();
            if self.scene_dirty {
                self.revision += 1;
                self.scene_dirty = false;
                changes.push(AttachProviderChange::Scene(self.scene()));
            }
            self.sequence += 1;
            changes.push(AttachProviderChange::StreamRepair(AttachStreamSnapshot {
                cursor: self.cursor()?,
                snapshot: bmux_terminal_grid::full_screen_repaint_bytes(self.grid.grid()),
            }));
            let delta = AttachProviderDelta {
                sequence: AttachDeltaSequence(self.sequence),
                base_view_revision: AttachViewRevision(base_revision),
                view_revision: AttachViewRevision(self.revision),
                changes,
                resume: self.resume()?,
            };
            self.continuity.apply_delta(&delta)?;
            Ok(AttachProviderEvent::Delta(delta))
        })
    }
    fn send_input(
        &mut self,
        input: AttachProviderInput,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            self.controls.validate_input(&input, &self.continuity)?;
            let protocol = self.grid.grid().protocol_state();
            let bytes = match input.payload {
                AttachInputPayload::Bytes(bytes) => bytes,
                AttachInputPayload::Paste(bytes) => {
                    if protocol.bracketed_paste {
                        [b"\x1b[200~".as_slice(), &bytes, b"\x1b[201~"].concat()
                    } else {
                        bytes
                    }
                }
                AttachInputPayload::Key { stroke, enhanced } => {
                    bmux_keyboard::encode::encode_key_with_modes(
                        &stroke,
                        enhanced,
                        bmux_keyboard::encode::KeyEncodingModes {
                            application_cursor: protocol.application_cursor,
                            application_keypad: protocol.application_keypad,
                        },
                    )
                    .context("unsupported key")
                    .map_err(failed)?
                }
                AttachInputPayload::Mouse(_) => {
                    return Ok(AttachProviderAck {
                        accepted: false,
                        message: Some(
                            "mouse reporting is not yet supported for this remote view".into(),
                        ),
                        command_id: None,
                    });
                }
            };
            let mut client = self
                .client
                .take()
                .ok_or_else(|| failed("control reconnect required; input was not sent"))?;
            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                execution::send(&mut client, self.execution.clone(), bytes),
            )
            .await
            .map_err(failed)?
            .map_err(failed)?;
            operation(result).map_err(failed)?;
            self.client = Some(client);
            Ok(Self::ack())
        })
    }
    fn update_viewport(
        &mut self,
        viewport: AttachProviderViewport,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            self.controls.validate_viewport(&viewport)?;
            self.pending_read = None;
            self.read_client = None;
            let mut client = self
                .client
                .take()
                .ok_or_else(|| failed("control reconnect required; resize was not sent"))?;
            let result = tokio::time::timeout(
                REQUEST_TIMEOUT,
                execution::resize(
                    &mut client,
                    self.execution.clone(),
                    viewport.columns.max(1),
                    viewport.rows.max(1),
                ),
            )
            .await
            .map_err(failed)?
            .map_err(failed)?;
            operation(result).map_err(failed)?;
            self.client = Some(client);
            self.pending_read = None;
            self.read_client = None;
            self.scene_dirty = true;
            Ok(Self::ack())
        })
    }
    fn execute_action(
        &mut self,
        action: AttachProviderAction,
    ) -> AttachSessionFuture<'_, AttachProviderAck> {
        Box::pin(async move {
            self.controls.validate_action(&action)?;
            let message = match action.action.as_str() {
                "focus" | "split" | "close" | "zoom" | "window-prev" | "window-next"
                | "window-goto" | "rename-tab" => {
                    "local layout keybinding is unavailable in a paired single-terminal view; no remote input was sent"
                }
                _ => "unsupported paired-terminal action; no remote input was sent",
            };
            Ok(AttachProviderAck {
                command_id: Some(action.command_id),
                accepted: false,
                message: Some(message.into()),
            })
        })
    }
    fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
        Box::pin(async move {
            self.detached = true;
            self.pending_read = None;
            self.read_client = None;
            self.client = None;
            Ok(self.controls.detach())
        })
    }
}
