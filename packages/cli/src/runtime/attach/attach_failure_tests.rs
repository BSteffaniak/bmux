//! The attach failure policy (`ipc_policy`): how each kind of failed server
//! request is classified, what it does to the attachment, and that the loop
//! steps it covers survive a slow server and stop cleanly on a lost
//! connection.
use super::super::ipc_policy::{
    AttachFailureDisposition, AttachIpcFailure, AttachOp, absorb_attach_failure,
    classify_attach_error, classify_attach_ipc_error,
};
use super::super::state::AttachLinkHealth;
use super::*;
use bmux_client::AttachOpenInfo;
use bmux_ipc::transport::ErasedIpcStream;
use bmux_ipc::{Envelope, EnvelopeKind, ErrorResponse, Response, ResponsePayload};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

fn server_error(code: bmux_ipc::ErrorCode, message: &str) -> ClientError {
    ClientError::ServerError {
        code,
        message: message.to_string(),
    }
}

fn io_error(kind: std::io::ErrorKind) -> ClientError {
    ClientError::Transport(bmux_ipc::transport::IpcTransportError::Io(
        std::io::Error::new(kind, "synthetic"),
    ))
}

fn view() -> AttachViewState {
    AttachViewState::new(AttachOpenInfo {
        context_id: Some(Uuid::from_u128(10)),
        session_id: Uuid::from_u128(1),
        can_write: true,
    })
}

// ── Classification: one case per class ──────────────────────────────────

#[test]
fn gone_covers_closed_streams_missing_runtimes_and_not_attached() {
    for error in [
        server_error(bmux_ipc::ErrorCode::NotFound, "session runtime not found"),
        server_error(
            bmux_ipc::ErrorCode::Internal,
            "not attached to session runtime",
        ),
        server_error(
            bmux_ipc::ErrorCode::InvalidRequest,
            "attach-layout-state failed: NotAttached",
        ),
    ] {
        assert_eq!(
            classify_attach_ipc_error(&error),
            AttachIpcFailure::Gone,
            "{error}"
        );
    }
}

#[test]
fn transient_covers_timeouts_and_an_unavailable_server() {
    for error in [
        ClientError::Timeout(Duration::from_secs(5)),
        server_error(bmux_ipc::ErrorCode::Timeout, "host gave up"),
        server_error(bmux_ipc::ErrorCode::Unavailable, "host unavailable"),
    ] {
        assert_eq!(
            classify_attach_ipc_error(&error),
            AttachIpcFailure::Transient,
            "{error}"
        );
    }
}

#[test]
fn connection_lost_covers_every_transport_io_failure() {
    for kind in [
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
    ] {
        assert_eq!(
            classify_attach_ipc_error(&io_error(kind)),
            AttachIpcFailure::ConnectionLost,
            "{kind:?}"
        );
    }
}

#[test]
fn rejected_covers_other_server_refusals() {
    assert_eq!(
        classify_attach_ipc_error(&server_error(bmux_ipc::ErrorCode::Internal, "boom")),
        AttachIpcFailure::Rejected
    );
    assert_eq!(
        classify_attach_ipc_error(&server_error(
            bmux_ipc::ErrorCode::NotFound,
            "attach-retarget-context failed: ContextNotFound"
        )),
        AttachIpcFailure::Rejected
    );
}

#[test]
fn protocol_covers_disagreeing_peers() {
    assert_eq!(
        classify_attach_ipc_error(&ClientError::Protocol("bad response kind".into())),
        AttachIpcFailure::Protocol
    );
    assert_eq!(
        classify_attach_ipc_error(&ClientError::UnexpectedResponse("expected ack")),
        AttachIpcFailure::Protocol
    );
}

#[test]
fn local_covers_failures_on_this_side() {
    assert_eq!(
        classify_attach_ipc_error(&ClientError::Local("encode failed".into())),
        AttachIpcFailure::Local
    );
    // A terminal failure in a step is this side's, never the server's.
    let terminal = anyhow::Error::from(std::io::Error::other("terminal write failed"));
    assert_eq!(classify_attach_error(&terminal), AttachIpcFailure::Local);
}

#[test]
fn step_failures_keep_their_class_through_wrapping() {
    // Readable attach messages, step labels and typed service errors all
    // still classify by the failure inside them.
    let readable =
        super::super::super::map_attach_client_error(ClientError::Timeout(Duration::from_secs(5)));
    assert_eq!(
        classify_attach_error(&readable),
        AttachIpcFailure::Transient
    );
    let readable_lost =
        super::super::super::map_attach_client_error(io_error(std::io::ErrorKind::UnexpectedEof));
    assert_eq!(
        classify_attach_error(&readable_lost),
        AttachIpcFailure::ConnectionLost
    );
    assert!(readable_lost.to_string().contains("connection lost"));
    let labelled: anyhow::Result<()> = Err(server_error(bmux_ipc::ErrorCode::Unavailable, "busy"))
        .context(AttachOp::ContextSwitch);
    assert_eq!(
        classify_attach_error(&labelled.unwrap_err()),
        AttachIpcFailure::Transient
    );
    let typed = anyhow::Error::from(bmux_plugin_sdk::TypedServiceClientError::from(
        bmux_plugin_sdk::TypedDispatchClientError::transport_failure(
            "clients-state",
            "current-client",
            bmux_plugin_sdk::TransportFailure::ConnectionLost(std::io::ErrorKind::BrokenPipe),
            "reader gone",
        ),
    ));
    assert_eq!(
        classify_attach_error(&typed),
        AttachIpcFailure::ConnectionLost
    );
    // A step failure with no typed error (an invalid action) is reported.
    assert_eq!(
        classify_attach_error(&anyhow::anyhow!("unknown action 'frobnicate'")),
        AttachIpcFailure::Rejected
    );
}

// ── The policy ──────────────────────────────────────────────────────────

#[test]
fn transient_failures_keep_running_resync_and_escalate_when_repeated() {
    let mut state = view();
    let timeout = || {
        anyhow::Error::from(ClientError::Timeout(Duration::from_secs(5)))
            .context(AttachOp::ContextSwitch)
    };
    let disposition = absorb_attach_failure(&mut state, AttachOp::ServerEvent, timeout());
    assert!(matches!(disposition, AttachFailureDisposition::Continue));
    assert_eq!(
        state.transient_status.as_deref(),
        Some("server slow: context switch timed out; retrying sync")
    );
    assert!(state.link.resync_due().is_some());
    for _ in 1..AttachLinkHealth::UNRESPONSIVE_AFTER {
        let _ = absorb_attach_failure(&mut state, AttachOp::ServerEvent, timeout());
    }
    assert!(state.link.unresponsive());
    assert!(
        state
            .transient_status
            .as_deref()
            .unwrap()
            .starts_with("server unresponsive: context switch timed out (3 in a row)"),
        "{:?}",
        state.transient_status
    );
    state.link.note_round_trip();
    assert_eq!(state.link.consecutive_transient, 0);
}

#[test]
fn lost_input_is_reported_as_lost() {
    let mut state = view();
    let error =
        super::super::super::map_attach_client_error(ClientError::Timeout(Duration::from_secs(5)));
    let failed: anyhow::Result<()> = Err(error).context(AttachOp::Input);
    let _ = absorb_attach_failure(&mut state, AttachOp::Action, failed.unwrap_err());
    assert_eq!(
        state.transient_status.as_deref(),
        Some("server slow: input timed out; input was not delivered")
    );
}

#[test]
fn rejections_are_shown_and_never_end_the_attachment() {
    let mut state = view();
    let disposition = absorb_attach_failure(
        &mut state,
        AttachOp::Action,
        anyhow::Error::from(server_error(
            bmux_ipc::ErrorCode::InvalidRequest,
            "not allowed",
        )),
    );
    assert!(matches!(disposition, AttachFailureDisposition::Continue));
    assert_eq!(
        state.transient_status.as_deref(),
        Some("action failed: not allowed")
    );
    assert!(state.link.resync_due().is_none());
}

#[test]
fn gone_lost_and_local_failures_end_the_attachment_their_own_way() {
    let mut state = view();
    assert!(matches!(
        absorb_attach_failure(
            &mut state,
            AttachOp::OutputDrain,
            anyhow::Error::from(server_error(
                bmux_ipc::ErrorCode::NotFound,
                "session runtime not found"
            )),
        ),
        AttachFailureDisposition::Exit(AttachExitReason::StreamClosed)
    ));
    assert!(matches!(
        absorb_attach_failure(
            &mut state,
            AttachOp::OutputDrain,
            anyhow::Error::from(io_error(std::io::ErrorKind::BrokenPipe)),
        ),
        AttachFailureDisposition::Exit(AttachExitReason::ConnectionLost)
    ));
    assert!(matches!(
        absorb_attach_failure(
            &mut state,
            AttachOp::Action,
            anyhow::Error::from(std::io::Error::other("terminal gone")),
        ),
        AttachFailureDisposition::Fatal(_)
    ));
}

#[test]
fn resync_attempts_back_off() {
    let mut link = AttachLinkHealth::default();
    link.request_resync();
    let first = link.resync_due().unwrap();
    link.request_resync();
    assert_eq!(
        link.resync_due(),
        Some(first),
        "a pending resync keeps its time"
    );
    let now = Instant::now();
    link.resync_failed(now);
    let after_one = link.resync_due().unwrap() - now;
    link.resync_failed(now);
    let after_two = link.resync_due().unwrap() - now;
    assert!(after_two > after_one);
    for _ in 0..10 {
        link.resync_failed(now);
    }
    assert!(link.resync_due().unwrap() - now <= Duration::from_secs(5));
    link.resync_succeeded();
    assert_eq!(link.resync_due(), None);
}

#[test]
fn reconnecting_wrappers_reconnect_when_the_server_went_away() {
    assert!(AttachExitReason::StreamClosed.server_went_away());
    assert!(AttachExitReason::ConnectionLost.server_went_away());
    assert!(!AttachExitReason::Detached.server_went_away());
    assert!(!AttachExitReason::Quit.server_went_away());
    assert_eq!(
        attach_exit_message(AttachExitReason::ConnectionLost),
        Some("attach ended: connection to server lost")
    );
    assert_eq!(attach_exit_status_code(AttachExitReason::ConnectionLost), 1);
}

/// The attach loop must never `?` a server request: every IPC-carrying step
/// goes through `attach_try!` (the attach failure policy).
#[test]
fn attach_loop_routes_server_requests_through_the_failure_policy() {
    let source = include_str!("runtime.rs");
    let start = source
        .find("'attach_loop: loop {")
        .expect("attach loop start");
    let end = source[start..]
        .find("// ── After 'attach_loop ──")
        .map(|offset| start + offset)
        .expect("attach loop end marker");
    let forbidden = [
        ".await?",
        "map_attach_client_error)?",
        "Err(map_attach_client_error",
    ];
    let offenders: Vec<_> = source[start..end]
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .filter(|(_, line)| forbidden.iter().any(|pattern| line.contains(pattern)))
        .map(|(index, line)| format!("loop line {}: {}", index + 1, line.trim()))
        .collect();
    assert!(
        offenders.is_empty(),
        "server requests in the attach loop must go through attach_try!:\n{}",
        offenders.join("\n")
    );
}

// ── Steps against a scripted server ─────────────────────────────────────

/// What the scripted server does with one request.
enum Reply {
    /// Answer a typed service call with this encoded result.
    Service(Vec<u8>),
    /// Answer with a server error.
    Error(bmux_ipc::ErrorCode, &'static str),
    /// Never answer (the client times out).
    Silence,
    /// Close the connection.
    Hangup,
}

fn service<T: serde::Serialize>(value: &T) -> Reply {
    Reply::Service(bmux_plugin_sdk::encode_service_message(value).unwrap())
}

/// A client whose server answers the handshake, then each request with the
/// next scripted reply; it records the operation of every service request.
async fn scripted_client(
    replies: Vec<Reply>,
    timeout: Duration,
) -> (StreamingBmuxClient, Arc<Mutex<Vec<String>>>) {
    let (client_stream, server_stream) = tokio::io::duplex(1 << 16);
    let operations = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&operations);
    tokio::spawn(async move {
        let mut replies = VecDeque::from(replies);
        let mut stream = ErasedIpcStream::new(Box::new(server_stream));
        let mut handshake = true;
        while let Ok(envelope) = stream.recv_envelope().await {
            let request: bmux_ipc::Request = bmux_ipc::decode(&envelope.payload).unwrap();
            let response = if handshake {
                handshake = false;
                let bmux_ipc::Request::Hello { contract, .. } = request else {
                    panic!("expected hello");
                };
                Response::Ok(ResponsePayload::HelloNegotiated {
                    negotiated: bmux_ipc::NegotiatedProtocol {
                        wire_epoch: contract.wire_epoch,
                        revision: contract.revisions.max,
                        capabilities: Vec::new(),
                    },
                })
            } else {
                if let bmux_ipc::Request::InvokeService { operation, .. } = &request {
                    seen.lock().unwrap().push(operation.clone());
                }
                match replies.pop_front().unwrap_or(Reply::Hangup) {
                    Reply::Service(payload) => {
                        Response::Ok(ResponsePayload::ServiceInvoked { payload })
                    }
                    Reply::Error(code, message) => Response::Err(ErrorResponse {
                        code,
                        message: message.to_string(),
                    }),
                    Reply::Silence => continue,
                    Reply::Hangup => return,
                }
            };
            if stream
                .send_envelope(&Envelope::new(
                    envelope.request_id,
                    EnvelopeKind::Response,
                    bmux_ipc::encode(&response).unwrap(),
                ))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let client = bmux_client::BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(client_stream)),
        timeout,
        "attach-failure-test",
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    (
        StreamingBmuxClient::from_client(client).unwrap(),
        operations,
    )
}

fn retarget_ready(session_id: Uuid, context_id: Uuid) -> Reply {
    service(&Ok::<_, attach_terminal_commands::AttachCommandError>(
        attach_terminal_commands::AttachRetargetReady {
            session_id,
            context_id: Some(context_id),
            can_write: false,
            cols: 80,
            rows: 24,
            top_inset: 0,
            right_inset: 0,
            bottom_inset: 0,
            left_inset: 0,
        },
    ))
}

#[tokio::test]
async fn retarget_keeps_the_confirmed_target_and_drops_the_stale_view_when_hydrate_fails() {
    let mut state = super::tests::attach_view_state_with_scrollback_fixture();
    state.attached_context_id = Some(Uuid::from_u128(10));
    assert!(state.cached_layout_state.is_some());
    let new_session = Uuid::from_u128(2);
    let new_context = Uuid::from_u128(20);
    let (mut client, operations) = scripted_client(
        vec![
            retarget_ready(new_session, new_context),
            Reply::Error(bmux_ipc::ErrorCode::Timeout, "host gave up"),
        ],
        Duration::from_secs(2),
    )
    .await;

    let error = retarget_attach_to_context(&mut client, &mut state, new_context)
        .await
        .expect_err("hydrate failed");

    assert_eq!(
        classify_attach_ipc_error(&error),
        AttachIpcFailure::Transient
    );
    // The server moved this client: the target is the one it confirmed,
    // never rolled back to a session it no longer has this client in...
    assert_eq!(state.attached_id, new_session);
    assert_eq!(state.attached_context_id, Some(new_context));
    assert!(!state.can_write);
    // ...and nothing of the old target is shown or sent input under it.
    assert!(state.cached_layout_state.is_none());
    assert!(state.pane_buffers.is_empty());
    // The confirmed target is rehydrated at the next opportunity; the
    // retarget request is never replayed.
    assert!(state.link.resync_due().is_some());
    assert!(!state.link.retarget_unconfirmed);
    assert_eq!(
        operations.lock().unwrap().as_slice(),
        ["attach-retarget-context", "attach-snapshot-state"]
    );
}

#[tokio::test]
async fn retarget_that_times_out_leaves_the_target_and_asks_the_server_at_resync() {
    let mut state = view();
    let old_session = state.attached_id;
    state.driver_geometry = Some(TerminalGeometry { cols: 80, rows: 24 });
    let server_session = Uuid::from_u128(3);
    let server_context = Uuid::from_u128(30);
    let (mut client, operations) = scripted_client(
        vec![
            Reply::Error(bmux_ipc::ErrorCode::Timeout, "request timed out after 5s"),
            // The resync asks the server what is attached...
            service(&Ok::<_, clients_state::ClientQueryError>(ClientSummary {
                id: Uuid::new_v4(),
                selected_session_id: Some(server_session),
                selected_context_id: Some(server_context),
                following_client_id: None,
                following_global: false,
            })),
            // ...publishes its viewport there, then rehydrates (still slow).
            service(&Ok::<_, attach_terminal_commands::AttachCommandError>(
                attach_terminal_commands::AttachViewportSet {
                    session_id: server_session,
                    cols: 80,
                    rows: 24,
                    top_inset: 0,
                    right_inset: 0,
                    bottom_inset: 0,
                    left_inset: 0,
                    context_id: Some(server_context),
                },
            )),
            Reply::Error(bmux_ipc::ErrorCode::Unavailable, "host unavailable"),
        ],
        Duration::from_secs(2),
    )
    .await;

    let error = retarget_attach_to_context(&mut client, &mut state, server_context)
        .await
        .expect_err("retarget timed out");
    assert_eq!(
        classify_attach_ipc_error(&error),
        AttachIpcFailure::Transient
    );
    assert_eq!(state.attached_id, old_session, "nothing changed locally");
    assert!(state.link.retarget_unconfirmed);
    assert!(state.link.resync_due().is_some());

    let resync = resync_attach_view(&mut client, &mut state)
        .await
        .expect_err("hydrate still slow");
    assert_eq!(classify_attach_error(&resync), AttachIpcFailure::Transient);
    assert_eq!(state.attached_id, server_session);
    assert_eq!(state.attached_context_id, Some(server_context));
    assert_eq!(
        operations.lock().unwrap().as_slice(),
        [
            "attach-retarget-context",
            "current-client",
            "attach-set-viewport",
            "attach-snapshot-state"
        ],
        "the retarget is never replayed"
    );
}

/// The context-change server event: a slow retarget keeps the attachment
/// running (the motivating crash).
#[tokio::test]
async fn context_switch_event_survives_a_slow_server() {
    let mut state = view();
    let self_client_id = Uuid::new_v4();
    let config = BmuxConfig::default();
    let mut processor = InputProcessor::new(attach_keymap_from_config(&config), false);
    let mut display_capture = DisplayCaptureFanout::default();
    let mut perf = perf_telemetry::PerfEventEmitter::new(
        perf_telemetry::PerfCaptureSettings::from_config(&config),
    );
    let mut pane_output_pending = false;
    let (mut client, _) = scripted_client(
        vec![Reply::Error(
            bmux_ipc::ErrorCode::Timeout,
            "request timed out after 5s",
        )],
        Duration::from_secs(2),
    )
    .await;
    let event = bmux_client::ServerEvent::PluginBusEvent {
        kind: bmux_contexts_plugin_api::contexts_events::EVENT_KIND
            .as_str()
            .to_string(),
        payload: serde_json::to_vec(
            &bmux_contexts_plugin_api::contexts_events::ContextEvent::SessionActiveContextChanged {
                session_id: state.attached_id,
                context_id: Uuid::from_u128(11),
                initiator_client_id: Some(self_client_id),
            },
        )
        .unwrap(),
    };
    let error = handle_attach_stream_server_event(
        event,
        &mut client,
        &mut processor,
        None,
        self_client_id,
        false,
        &[],
        &mut state,
        &mut display_capture,
        None,
        &config,
        &mut perf,
        &mut pane_output_pending,
    )
    .await
    .err()
    .expect("retarget timed out");

    assert!(matches!(
        absorb_attach_failure(&mut state, AttachOp::ServerEvent, error),
        AttachFailureDisposition::Continue
    ));
    assert_eq!(
        state.transient_status.as_deref(),
        Some("server slow: context switch timed out; retrying sync")
    );
    assert_eq!(state.attached_id, Uuid::from_u128(1));
}

#[tokio::test]
async fn resize_survives_a_request_timeout() {
    let mut state = view();
    let (mut client, _) = scripted_client(vec![Reply::Silence], Duration::from_millis(50)).await;
    let error = update_attach_viewport_with_geometry(
        &mut client,
        &state,
        TerminalGeometry {
            cols: 100,
            rows: 30,
        },
    )
    .await
    .expect_err("server never answers");
    assert!(matches!(error, ClientError::Timeout(_)), "{error:?}");
    assert!(matches!(
        absorb_attach_failure(&mut state, AttachOp::Resize, error.into()),
        AttachFailureDisposition::Continue
    ));
    assert_eq!(
        state.transient_status.as_deref(),
        Some("server slow: resize timed out; retrying sync")
    );
}

#[tokio::test]
async fn output_drain_survives_an_unavailable_server_and_stops_on_a_lost_connection() {
    let mut state = view();
    let pane = Uuid::new_v4();
    let (mut client, _) = scripted_client(
        vec![
            Reply::Error(bmux_ipc::ErrorCode::Unavailable, "host unavailable"),
            Reply::Hangup,
        ],
        Duration::from_secs(2),
    )
    .await;
    let slow = attach_pane_frames_streaming(&mut client, state.attached_id, vec![pane], 0)
        .await
        .expect_err("unavailable");
    assert!(matches!(
        absorb_attach_failure(&mut state, AttachOp::OutputDrain, slow.into()),
        AttachFailureDisposition::Continue
    ));

    let lost = attach_pane_frames_streaming(&mut client, state.attached_id, vec![pane], 0)
        .await
        .expect_err("hung up");
    assert!(matches!(
        absorb_attach_failure(&mut state, AttachOp::OutputDrain, lost.into()),
        AttachFailureDisposition::Exit(AttachExitReason::ConnectionLost)
    ));
}

// ── Native provider path ────────────────────────────────────────────────

/// A terminal that counts restorations.
struct CountingTerminal {
    inner: HeadlessAttachTerminal,
    restored: Arc<AtomicUsize>,
}

impl Write for CountingTerminal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl AttachTerminal for CountingTerminal {
    fn geometry(&self) -> TerminalGeometry {
        self.inner.geometry()
    }

    fn enter_attach_mode(
        &mut self,
        kitty_keyboard_enabled: bool,
        mouse_capture_enabled: bool,
        outer_paste_capture_enabled: bool,
    ) -> Result<bool> {
        self.inner.enter_attach_mode(
            kitty_keyboard_enabled,
            mouse_capture_enabled,
            outer_paste_capture_enabled,
        )
    }

    fn next_event(&mut self) -> AttachTerminalEventFuture<'_> {
        self.inner.next_event()
    }

    fn restore_after_attach_ui(&mut self) -> Result<()> {
        self.restored.fetch_add(1, Ordering::SeqCst);
        self.inner.restore_after_attach_ui()
    }

    #[cfg(any(
        feature = "image-sixel",
        feature = "image-kitty",
        feature = "image-iterm2"
    ))]
    fn detect_image_capabilities(&mut self) -> bmux_image::host_caps::HostImageCapabilities {
        self.inner.detect_image_capabilities()
    }
}

/// A provider session that fails its event stream and every request.
#[derive(Debug)]
struct FailingSession {
    event_error: bmux_client::AttachSessionError,
}

impl AttachSession for FailingSession {
    fn initial_snapshot(
        &mut self,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderSnapshot> {
        Box::pin(async {
            Ok(bmux_client::AttachProviderSnapshot {
                view_revision: bmux_client::AttachViewRevision(1),
                event_sequence: bmux_client::AttachDeltaSequence(1),
                scene: bmux_attach_layout_protocol::AttachScene {
                    session_id: Uuid::nil(),
                    focus: bmux_attach_layout_protocol::AttachFocusTarget::None,
                    surfaces: Vec::new(),
                },
                streams: Vec::new(),
                resume: bmux_client::AttachResumeState {
                    view_revision: bmux_client::AttachViewRevision(1),
                    event_sequence: bmux_client::AttachDeltaSequence(1),
                    streams: Vec::new(),
                    provider_token: Vec::new(),
                },
            })
        })
    }

    fn next_event(&mut self) -> bmux_client::AttachSessionFuture<'_, AttachProviderEvent> {
        let error = self.event_error.clone();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Err(error)
        })
    }

    fn send_input(
        &mut self,
        _input: AttachProviderInput,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderAck> {
        Box::pin(async {
            Err(bmux_client::AttachSessionError::Provider {
                reason: "slow".into(),
            })
        })
    }

    fn update_viewport(
        &mut self,
        _viewport: AttachProviderViewport,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderAck> {
        Box::pin(async {
            Ok(bmux_client::AttachProviderAck {
                command_id: None,
                accepted: true,
                message: None,
            })
        })
    }

    fn execute_action(
        &mut self,
        _action: bmux_client::AttachProviderAction,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderAck> {
        Box::pin(async {
            Err(bmux_client::AttachSessionError::Provider {
                reason: "slow".into(),
            })
        })
    }

    fn detach(&mut self) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachDetachOutcome> {
        Box::pin(async { Err(bmux_client::AttachSessionError::Detached) })
    }
}

#[tokio::test]
async fn native_provider_stops_cleanly_with_connection_lost_and_restores_the_terminal() {
    let (inner, handle) = HeadlessAttachTerminal::new(80, 24);
    let restored = Arc::new(AtomicUsize::new(0));
    let mut terminal = CountingTerminal {
        inner,
        restored: Arc::clone(&restored),
    };
    // A failed request (here a resize) loses only that request.
    handle.resize(100, 30).unwrap();
    let outcome = run_native_attach_session_with_terminal(
        Box::new(FailingSession {
            event_error: bmux_client::AttachSessionError::Provider {
                reason: "transport closed".into(),
            },
        }),
        &mut terminal,
    )
    .await
    .expect("a lost provider connection is a clean stop");
    assert_eq!(outcome.exit_reason, AttachExitReason::ConnectionLost);
    assert_eq!(outcome.status_code, 1);
    assert!(restored.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn native_provider_error_exits_still_restore_the_terminal() {
    let (inner, _handle) = HeadlessAttachTerminal::new(80, 24);
    let restored = Arc::new(AtomicUsize::new(0));
    let mut terminal = CountingTerminal {
        inner,
        restored: Arc::clone(&restored),
    };
    let error = run_native_attach_session_with_terminal(
        Box::new(FailingSession {
            event_error: bmux_client::AttachSessionError::SequenceGap {
                expected: 2,
                actual: 9,
            },
        }),
        &mut terminal,
    )
    .await
    .expect_err("a continuity violation cannot be recovered on the native path");
    assert!(error.to_string().contains("sequence gap"), "{error:#}");
    assert!(restored.load(Ordering::SeqCst) >= 1);
}
