use anyhow::Result;
use bmux_client::{
    AttachProvider, AttachProviderBackend, AttachProviderError, AttachProviderFuture,
    AttachProviderRegistration, AttachProviderSession, AttachTarget, ResolvedAttachTarget,
    global_attach_provider_registry,
};
use std::any::Any;
use std::sync::{Arc, OnceLock};

const PROVIDER_ID: &str = "bmux.pane-runtime";

#[derive(Debug)]
struct PaneRuntimeAttachProvider;

#[derive(Debug)]
struct PaneRuntimeAttachTarget {
    target: Option<String>,
}

impl ResolvedAttachTarget for PaneRuntimeAttachTarget {
    fn provider_id(&self) -> &str {
        PROVIDER_ID
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl AttachProvider for PaneRuntimeAttachProvider {
    fn id(&self) -> &str {
        PROVIDER_ID
    }

    fn supports(&self, target: &AttachTarget) -> bool {
        target.scheme().is_none() || target.scheme() == Some("local")
    }

    fn requires_fallback_client(&self) -> bool {
        true
    }

    fn resolve(
        &self,
        target: &AttachTarget,
    ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
        let resolved = match target.scheme() {
            Some("local") => target.reference(),
            None => target.raw(),
            Some(_) => {
                return Err(AttachProviderError::InvalidTarget {
                    provider_id: PROVIDER_ID.to_string(),
                    target: target.raw().to_string(),
                    reason: "unsupported target scheme".to_string(),
                });
            }
        };
        Ok(Arc::new(PaneRuntimeAttachTarget {
            target: (!resolved.is_empty()).then(|| resolved.to_string()),
        }))
    }

    fn open(
        &self,
        resolved: Arc<dyn ResolvedAttachTarget>,
        _resume: Option<bmux_client::AttachResumeState>,
        context: bmux_client::AttachProviderOpenContext,
    ) -> AttachProviderFuture<'_, AttachProviderSession> {
        let fallback_client = context.fallback_client;
        Box::pin(async move {
            let target = resolved
                .as_any()
                .downcast_ref::<PaneRuntimeAttachTarget>()
                .ok_or_else(|| AttachProviderError::InvalidTarget {
                    provider_id: PROVIDER_ID.to_string(),
                    target: String::new(),
                    reason: format!(
                        "resolved plan belongs to provider '{}'",
                        resolved.provider_id()
                    ),
                })?;
            let client = fallback_client.ok_or_else(|| AttachProviderError::OpenFailed {
                provider_id: PROVIDER_ID.to_string(),
                reason: "pane-runtime provider requires the fallback client".to_string(),
            })?;
            Ok(AttachProviderSession {
                backend: AttachProviderBackend::Legacy(client),
                target: target.target.clone(),
            })
        })
    }
}

pub struct ResolvedProviderAttach {
    provider: Arc<dyn AttachProvider>,
    resolved: Arc<dyn ResolvedAttachTarget>,
}

impl ResolvedProviderAttach {
    #[must_use]
    pub fn requires_fallback_client(&self) -> bool {
        self.provider.requires_fallback_client()
    }

    pub async fn open(
        self,
        resume: Option<bmux_client::AttachResumeState>,
        context: bmux_client::AttachProviderOpenContext,
    ) -> Result<AttachProviderSession> {
        self.provider
            .open(self.resolved, resume, context)
            .await
            .map_err(anyhow::Error::from)
    }
}

pub fn resolve(target: Option<&str>) -> Result<ResolvedProviderAttach> {
    install();
    let target = AttachTarget::parse(target.unwrap_or_default());
    let provider = global_attach_provider_registry()
        .resolve(&target)
        .map_err(anyhow::Error::from)?;
    let resolved = provider.resolve(&target).map_err(anyhow::Error::from)?;
    Ok(ResolvedProviderAttach { provider, resolved })
}

/// Install the existing pane-runtime attach path as the local/bare fallback.
pub fn install() {
    static REGISTRATION: OnceLock<AttachProviderRegistration> = OnceLock::new();
    REGISTRATION.get_or_init(|| {
        global_attach_provider_registry()
            .register(Arc::new(PaneRuntimeAttachProvider))
            .expect("pane-runtime attach provider ID must be unique")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachLayer, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
    };
    use bmux_client::{
        AttachDeltaSequence, AttachDetachOutcome, AttachInputPayload, AttachProviderAck,
        AttachProviderBackend, AttachProviderChange, AttachProviderDelta, AttachProviderEvent,
        AttachProviderInput, AttachProviderSnapshot, AttachProviderViewport, AttachResumeState,
        AttachSession, AttachSessionFuture, AttachStreamCursor, AttachStreamId,
        AttachStreamSnapshot, AttachViewRevision, BmuxClient,
    };
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Debug)]
    struct SyntheticResolvedTarget;

    impl ResolvedAttachTarget for SyntheticResolvedTarget {
        fn provider_id(&self) -> &'static str {
            "test.synthetic"
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Debug, Default)]
    struct SyntheticState {
        viewports: Vec<AttachProviderViewport>,
        inputs: Vec<AttachProviderInput>,
        actions: Vec<bmux_client::AttachProviderAction>,
        detached: usize,
    }

    #[derive(Debug)]
    struct SyntheticSession {
        state: Arc<Mutex<SyntheticState>>,
        event_index: usize,
    }

    fn synthetic_scene() -> AttachScene {
        AttachScene {
            session_id: Uuid::nil(),
            focus: AttachFocusTarget::Surface {
                surface_id: Uuid::nil(),
            },
            surfaces: vec![AttachSurface {
                id: Uuid::nil(),
                kind: AttachSurfaceKind::Pane,
                layer: AttachLayer::Pane,
                z: 0,
                rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                content_rect: AttachRect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 24,
                },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
                pane_id: Some(Uuid::nil()),
            }],
        }
    }

    fn synthetic_cursor(offset: u64) -> AttachStreamCursor {
        AttachStreamCursor {
            stream_id: AttachStreamId::new("synthetic-stream").unwrap(),
            surface_id: Uuid::nil(),
            generation: 1,
            offset,
        }
    }

    fn synthetic_snapshot() -> AttachProviderSnapshot {
        AttachProviderSnapshot {
            view_revision: AttachViewRevision(1),
            event_sequence: AttachDeltaSequence(1),
            scene: synthetic_scene(),
            streams: vec![AttachStreamSnapshot {
                cursor: synthetic_cursor(5),
                snapshot: b"hello".to_vec(),
            }],
            resume: AttachResumeState {
                view_revision: AttachViewRevision(1),
                event_sequence: AttachDeltaSequence(1),
                streams: vec![synthetic_cursor(5)],
                provider_token: b"resume".to_vec(),
            },
        }
    }

    impl AttachSession for SyntheticSession {
        fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
            Box::pin(async { Ok(synthetic_snapshot()) })
        }

        fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent> {
            Box::pin(async move {
                let event_index = self.event_index;
                self.event_index += 1;
                if event_index == 0 {
                    Ok(AttachProviderEvent::Delta(AttachProviderDelta {
                        sequence: AttachDeltaSequence(2),
                        base_view_revision: AttachViewRevision(1),
                        view_revision: AttachViewRevision(1),
                        changes: vec![AttachProviderChange::StreamAppend {
                            cursor: synthetic_cursor(5),
                            end_offset: 6,
                            bytes: b"!".to_vec(),
                        }],
                        resume: AttachResumeState {
                            view_revision: AttachViewRevision(1),
                            event_sequence: AttachDeltaSequence(2),
                            streams: vec![synthetic_cursor(6)],
                            provider_token: b"resume-2".to_vec(),
                        },
                    }))
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    Ok(AttachProviderEvent::Detached)
                }
            })
        }

        fn send_input(
            &mut self,
            input: AttachProviderInput,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            self.state.lock().unwrap().inputs.push(input);
            Box::pin(async {
                Ok(AttachProviderAck {
                    command_id: None,
                    accepted: true,
                    message: None,
                })
            })
        }

        fn update_viewport(
            &mut self,
            viewport: AttachProviderViewport,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            self.state.lock().unwrap().viewports.push(viewport);
            Box::pin(async {
                Ok(AttachProviderAck {
                    command_id: None,
                    accepted: true,
                    message: None,
                })
            })
        }

        fn execute_action(
            &mut self,
            action: bmux_client::AttachProviderAction,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            self.state.lock().unwrap().actions.push(action.clone());
            Box::pin(async move {
                Ok(AttachProviderAck {
                    command_id: Some(action.command_id),
                    accepted: true,
                    message: None,
                })
            })
        }

        fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
            self.state.lock().unwrap().detached += 1;
            Box::pin(async { Ok(AttachDetachOutcome::Detached) })
        }
    }

    #[derive(Debug)]
    struct DisconnectSession {
        mismatch: bool,
        detached: Arc<Mutex<usize>>,
    }

    impl AttachSession for DisconnectSession {
        fn initial_snapshot(&mut self) -> AttachSessionFuture<'_, AttachProviderSnapshot> {
            Box::pin(async { Ok(synthetic_snapshot()) })
        }

        fn next_event(&mut self) -> AttachSessionFuture<'_, AttachProviderEvent> {
            let mismatch = self.mismatch;
            Box::pin(async move {
                let mut resume = AttachResumeState {
                    view_revision: AttachViewRevision(1),
                    event_sequence: AttachDeltaSequence(1),
                    streams: vec![synthetic_cursor(5)],
                    provider_token: b"resume".to_vec(),
                };
                if mismatch {
                    resume.event_sequence = AttachDeltaSequence(99);
                }
                Ok(AttachProviderEvent::Disconnected(
                    bmux_client::AttachProviderDisconnect {
                        recoverable: true,
                        reason: "synthetic disconnect".to_string(),
                        resume: Some(resume),
                        retry_after_ms: Some(10),
                    },
                ))
            })
        }

        fn send_input(
            &mut self,
            _input: AttachProviderInput,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            unreachable!("disconnect session receives no input")
        }

        fn update_viewport(
            &mut self,
            _viewport: AttachProviderViewport,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            Box::pin(async {
                Ok(AttachProviderAck {
                    command_id: None,
                    accepted: true,
                    message: None,
                })
            })
        }

        fn execute_action(
            &mut self,
            _action: bmux_client::AttachProviderAction,
        ) -> AttachSessionFuture<'_, AttachProviderAck> {
            unreachable!("disconnect session receives no actions")
        }

        fn detach(&mut self) -> AttachSessionFuture<'_, AttachDetachOutcome> {
            *self.detached.lock().unwrap() += 1;
            Box::pin(async { Ok(AttachDetachOutcome::Detached) })
        }
    }

    #[tokio::test]
    async fn native_runner_preserves_validated_resume_on_recoverable_disconnect() {
        let detached = Arc::new(Mutex::new(0));
        let (mut terminal, _handle) = super::super::runtime::HeadlessAttachTerminal::new(80, 24);
        let outcome = super::super::runtime::run_native_attach_session_with_terminal(
            Box::new(DisconnectSession {
                mismatch: false,
                detached: Arc::clone(&detached),
            }),
            &mut terminal,
        )
        .await
        .expect("recoverable disconnect");
        assert_eq!(
            outcome.exit_reason,
            super::super::state::AttachExitReason::StreamClosed
        );
        assert_eq!(outcome.status_code, 0);
        let resume = outcome.resume.expect("resume state");
        assert_eq!(resume.provider_token, b"resume");
        assert_eq!(resume.streams, vec![synthetic_cursor(5)]);
        assert_eq!(*detached.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn native_runner_rejects_mismatched_disconnect_resume() {
        let (mut terminal, _handle) = super::super::runtime::HeadlessAttachTerminal::new(80, 24);
        let error = super::super::runtime::run_native_attach_session_with_terminal(
            Box::new(DisconnectSession {
                mismatch: true,
                detached: Arc::new(Mutex::new(0)),
            }),
            &mut terminal,
        )
        .await
        .expect_err("mismatched resume must fail");
        assert!(error.to_string().contains("resume state did not match"));
    }

    #[derive(Debug)]
    struct UnreachableConnector;

    impl bmux_client::AttachEndpointConnector for UnreachableConnector {
        fn connect<'a>(
            &'a self,
            _target: &'a str,
            _client_name: &'static str,
        ) -> bmux_client::AttachEndpointConnectFuture<'a> {
            Box::pin(async { Err("synthetic connector is not dialed".to_string()) })
        }
    }

    #[derive(Debug)]
    struct SyntheticProvider {
        state: Arc<Mutex<SyntheticState>>,
    }

    impl AttachProvider for SyntheticProvider {
        fn id(&self) -> &'static str {
            "test.synthetic"
        }

        fn supports(&self, target: &AttachTarget) -> bool {
            target.scheme() == Some("synthetic")
        }

        fn resolve(
            &self,
            _target: &AttachTarget,
        ) -> Result<Arc<dyn ResolvedAttachTarget>, AttachProviderError> {
            Ok(Arc::new(SyntheticResolvedTarget))
        }

        fn open(
            &self,
            _resolved: Arc<dyn ResolvedAttachTarget>,
            _resume: Option<AttachResumeState>,
            context: bmux_client::AttachProviderOpenContext,
        ) -> AttachProviderFuture<'_, AttachProviderSession> {
            assert!(context.fallback_client.is_none());
            assert!(context.endpoint_connector.is_some());
            let state = Arc::clone(&self.state);
            Box::pin(async move {
                Ok(AttachProviderSession {
                    backend: AttachProviderBackend::Session(Box::new(SyntheticSession {
                        state,
                        event_index: 0,
                    })),
                    target: None,
                })
            })
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // End-to-end provider fixture verifies action dedupe, modal input, rendering, paste, and detach lifecycle.
    async fn synthetic_provider_runs_through_domain_neutral_cli_path() {
        install();
        let state = Arc::new(Mutex::new(SyntheticState::default()));
        let registration = global_attach_provider_registry()
            .register(Arc::new(SyntheticProvider {
                state: Arc::clone(&state),
            }))
            .expect("register synthetic provider");
        let provider = resolve(Some("synthetic://workspace")).expect("resolve synthetic provider");
        assert!(!provider.requires_fallback_client());
        let opened = provider
            .open(
                None,
                bmux_client::AttachProviderOpenContext {
                    fallback_client: None,
                    endpoint_connector: Some(Arc::new(UnreachableConnector)),
                },
            )
            .await
            .expect("open synthetic provider");
        let mut native_session = match opened.backend {
            AttachProviderBackend::Session(session) => session,
            AttachProviderBackend::Legacy(_) => panic!("expected native provider session"),
        };
        let mut action_controls = bmux_client::AttachControlValidator::default();
        let action = bmux_client::AttachProviderAction {
            command_id: "action-1".to_string(),
            action: "focus-next".to_string(),
            arguments: Vec::new(),
        };
        super::super::runtime::execute_native_provider_action(
            native_session.as_mut(),
            &mut action_controls,
            action.clone(),
        )
        .await
        .expect("execute generic action");
        let duplicate = super::super::runtime::execute_native_provider_action(
            native_session.as_mut(),
            &mut action_controls,
            action,
        )
        .await
        .expect_err("duplicate action must be rejected");
        assert!(duplicate.to_string().contains("duplicate attach action"));
        let (mut terminal, handle) = super::super::runtime::HeadlessAttachTerminal::new(80, 24);
        handle
            .send_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('a'),
                    crossterm::event::KeyModifiers::CONTROL,
                ),
            ))
            .unwrap();
        handle
            .send_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Esc,
                    crossterm::event::KeyModifiers::NONE,
                ),
            ))
            .unwrap();
        handle
            .send_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char('o'),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ))
            .unwrap();
        handle
            .send_event(crossterm::event::Event::Paste("input".to_string()))
            .unwrap();
        let outcome = super::super::runtime::run_native_attach_session_with_terminal(
            native_session,
            &mut terminal,
        )
        .await
        .expect("run synthetic provider");
        assert_eq!(
            outcome.exit_reason,
            super::super::state::AttachExitReason::Detached
        );
        let output = handle.output_bytes();
        assert!(
            output
                .windows(b"hello!".len())
                .any(|window| window == b"hello!"),
            "rendered provider output did not contain expected bytes"
        );
        let state = state.lock().unwrap();
        assert!(!state.viewports.is_empty());
        assert!(
            state
                .viewports
                .iter()
                .all(|viewport| viewport.columns == 80)
        );
        assert!(!state.inputs.is_empty());
        assert!(state.inputs.iter().all(|input| input.generation == 1));
        assert_eq!(
            state.inputs.last().map(|input| &input.payload),
            Some(&AttachInputPayload::Paste(b"input".to_vec()))
        );
        assert_eq!(state.detached, 1);
        drop(state);
        drop(registration);
    }

    #[test]
    fn supports_bare_and_local_targets_only() {
        let provider = PaneRuntimeAttachProvider;
        assert!(provider.supports(&AttachTarget::parse("main")));
        assert!(provider.supports(&AttachTarget::parse("local://main")));
        assert!(!provider.supports(&AttachTarget::parse("synthetic://main")));
    }

    #[test]
    fn resolution_strips_local_scheme_preserves_bare_and_supports_follow() {
        let provider = PaneRuntimeAttachProvider;
        for (raw, expected) in [
            ("main", Some("main")),
            ("local://main", Some("main")),
            ("", None),
        ] {
            let resolved = provider.resolve(&AttachTarget::parse(raw)).unwrap();
            let resolved = resolved
                .as_any()
                .downcast_ref::<PaneRuntimeAttachTarget>()
                .unwrap();
            assert_eq!(resolved.target.as_deref(), expected);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_preserves_existing_connected_client_and_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = bmux_config::ConfigPaths::new(
            root.path().join("config"),
            root.path().join("runtime"),
            root.path().join("data"),
            root.path().join("state"),
        );
        let server = Arc::new(bmux_server::BmuxServer::from_config_paths(&paths));
        let running = Arc::clone(&server);
        let task = tokio::spawn(async move { running.run().await });
        for _ in 0..100 {
            if paths.server_socket().exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = BmuxClient::connect_with_paths(&paths, "attach-provider-test")
            .await
            .expect("connect client");
        let principal_id = client.principal_id();
        let provider = PaneRuntimeAttachProvider;
        let resolved = provider.resolve(&AttachTarget::parse("main")).unwrap();
        let session = provider
            .open(
                resolved,
                None,
                bmux_client::AttachProviderOpenContext {
                    fallback_client: Some(client),
                    endpoint_connector: None,
                },
            )
            .await
            .expect("open default provider");
        assert_eq!(session.target.as_deref(), Some("main"));
        let AttachProviderBackend::Legacy(client) = session.backend else {
            panic!("pane-runtime provider must preserve the legacy backend");
        };
        assert_eq!(client.principal_id(), principal_id);

        server.request_shutdown();
        task.await.expect("server join").expect("server run");
    }
}
