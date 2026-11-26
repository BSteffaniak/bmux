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
fn the_server_going_away_ends_attachments_that_cannot_reconnect() {
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

// ── The connection banner ───────────────────────────────────────────────

#[test]
fn the_degraded_banner_shows_while_the_server_is_unresponsive() {
    use super::super::connection_banner::ConnectionBanner;
    let mut state = view();
    let timeout = || anyhow::Error::from(ClientError::Timeout(Duration::from_secs(5)));
    for _ in 1..AttachLinkHealth::UNRESPONSIVE_AFTER {
        let _ = absorb_attach_failure(&mut state, AttachOp::Resize, timeout());
        assert_eq!(state.link.banner(), None, "slow is not yet unresponsive");
    }
    let _ = absorb_attach_failure(&mut state, AttachOp::Resize, timeout());
    let banner = state.link.banner().expect("unresponsive");
    assert_eq!(banner, ConnectionBanner::Degraded);
    assert!(!banner.modal(), "a degraded link still takes input");
    // A request that completes clears it.
    state.link.note_round_trip();
    assert_eq!(state.link.banner(), None);
    // So does a resync that succeeds.
    for _ in 0..AttachLinkHealth::UNRESPONSIVE_AFTER {
        let _ = absorb_attach_failure(&mut state, AttachOp::Resize, timeout());
    }
    assert!(state.link.banner().is_some());
    state.link.resync_succeeded();
    assert_eq!(state.link.banner(), None);
}

fn rejected_resync(state: &mut AttachViewState) {
    let error = anyhow::Error::from(server_error(bmux_ipc::ErrorCode::InvalidRequest, "denied"));
    assert!(matches!(
        absorb_attach_failure(state, AttachOp::Resync, error),
        AttachFailureDisposition::Continue
    ));
    state.link.resync_failed(Instant::now());
}

#[test]
fn repeated_rejected_resyncs_stall_until_the_user_retries() {
    use super::super::connection_banner::{BannerAction, ConnectionBanner};
    let mut state = view();
    state.link.request_resync();
    for _ in 1..AttachLinkHealth::STALL_AFTER_REJECTED_RESYNCS {
        rejected_resync(&mut state);
        assert!(state.link.resync_due().is_some(), "retried on its own");
        assert_eq!(state.link.banner(), None);
    }
    rejected_resync(&mut state);
    // Capped: no more automatic attempts; the banner says why and takes keys.
    assert_eq!(state.link.resync_due(), None);
    let banner = state.link.banner().expect("stalled");
    assert_eq!(
        banner,
        ConnectionBanner::Stalled {
            reason: "resync failed: denied".to_owned()
        }
    );
    assert!(banner.modal());
    assert_eq!(banner.action_for('r'), Some(BannerAction::Retry));
    assert!(banner.text().contains("press r to retry"));

    // r: one attempt now; refused again, it stalls again at once.
    state.link.retry_stalled();
    assert_eq!(state.link.banner(), None);
    assert!(state.link.resync_due().is_some());
    rejected_resync(&mut state);
    assert!(state.link.banner().is_some());
    assert_eq!(state.link.resync_due(), None);

    // A resync that succeeds ends the stall.
    state.link.retry_stalled();
    state.link.resync_succeeded();
    assert_eq!(state.link.banner(), None);
    assert_eq!(state.link.consecutive_rejected_resyncs, 0);
}

#[test]
fn only_rejections_in_a_row_stall_resyncs() {
    let mut state = view();
    state.link.request_resync();
    for _ in 1..AttachLinkHealth::STALL_AFTER_REJECTED_RESYNCS {
        rejected_resync(&mut state);
    }
    // A slow server in between: the refusals are no longer in a row.
    let _ = absorb_attach_failure(
        &mut state,
        AttachOp::Resync,
        anyhow::Error::from(ClientError::Timeout(Duration::from_secs(5))),
    );
    state.link.resync_failed(Instant::now());
    rejected_resync(&mut state);
    assert!(state.link.stalled.is_none());
    assert!(state.link.resync_due().is_some());
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

/// A provider snapshot of an empty scene.
fn empty_provider_snapshot() -> bmux_client::AttachProviderSnapshot {
    bmux_client::AttachProviderSnapshot {
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
    }
}

impl AttachSession for FailingSession {
    fn initial_snapshot(
        &mut self,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderSnapshot> {
        Box::pin(async { Ok(empty_provider_snapshot()) })
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
        None,
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
        None,
    )
    .await
    .expect_err("a continuity violation cannot be recovered on the native path");
    assert!(error.to_string().contains("sequence gap"), "{error:#}");
    assert!(restored.load(Ordering::SeqCst) >= 1);
}

// ── Reconnecting in place, end to end ───────────────────────────────────

/// A fake server for a whole attachment: it admits the client to one session
/// and serves its layout and snapshot (one empty pane); anything else is
/// answered "not found". It records the operations it served and hangs up
/// when `kill` is notified.
struct FakeAttachServer {
    operations: Arc<Mutex<Vec<String>>>,
    kill: Arc<tokio::sync::Notify>,
}

/// Whether the fake server still has the session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FakeSession {
    Present,
    Gone,
}

const FAKE_SESSION: Uuid = Uuid::from_u128(0x5e55);
const FAKE_CONTEXT: Uuid = Uuid::from_u128(0xc0de);
const FAKE_PANE: Uuid = Uuid::from_u128(0x9a4e);

fn fake_layout_payload(zoomed: Option<bool>) -> Vec<u8> {
    use bmux_attach_layout_protocol::{
        AttachFocusTarget, AttachRect, AttachScene, AttachSurface, AttachSurfaceKind,
        PaneLayoutNode, PaneState, PaneSummary,
    };
    let mut payload = serde_json::json!({
        "panes": [PaneSummary {
            id: FAKE_PANE,
            index: 1,
            name: None,
            focused: true,
            state: PaneState::Running,
            state_reason: None,
        }],
        "layout_root": PaneLayoutNode::Leaf { pane_id: FAKE_PANE },
        "scene": AttachScene {
            session_id: FAKE_SESSION,
            focus: AttachFocusTarget::Pane { pane_id: FAKE_PANE },
            surfaces: vec![AttachSurface {
                id: Uuid::from_u128(0x5f),
                kind: AttachSurfaceKind::Pane,
                layer: bmux_attach_layout_protocol::AttachLayer::Pane,
                z: 0,
                pane_id: Some(FAKE_PANE),
                rect: AttachRect { x: 0, y: 0, w: 80, h: 24 },
                content_rect: AttachRect { x: 1, y: 1, w: 78, h: 22 },
                interactive_regions: Vec::new(),
                opaque: true,
                visible: true,
                accepts_input: true,
                cursor_owner: true,
            }],
        },
    });
    if let Some(zoomed) = zoomed {
        payload["zoomed"] = serde_json::Value::Bool(zoomed);
    }
    serde_json::to_vec(&payload).unwrap()
}

fn fake_reply(operation: &str, session: FakeSession) -> Response {
    use bmux_pane_runtime_plugin_api::attach_runtime_state as state;
    let ok = |payload: Vec<u8>| Response::Ok(ResponsePayload::ServiceInvoked { payload });
    match operation {
        "current-client" => ok(bmux_plugin_sdk::encode_service_message(&Ok::<
            _,
            clients_state::ClientQueryError,
        >(
            ClientSummary {
                id: Uuid::from_u128(0xc11e),
                selected_session_id: Some(FAKE_SESSION),
                selected_context_id: Some(FAKE_CONTEXT),
                following_client_id: None,
                following_global: false,
            },
        ))
        .unwrap()),
        "attach-session" | "attach-context" => {
            ok(bmux_plugin_sdk::encode_service_message(&match session {
                FakeSession::Present => Ok(attach_terminal_commands::AttachGrant {
                    token: Uuid::new_v4(),
                    session_id: FAKE_SESSION,
                    context_id: Some(FAKE_CONTEXT),
                    expires_epoch_ms: u64::MAX,
                }),
                FakeSession::Gone => {
                    Err(attach_terminal_commands::AttachCommandError::ContextNotFound)
                }
            })
            .unwrap())
        }
        "attach-open" => ok(bmux_plugin_sdk::encode_service_message(&Ok::<
            _,
            attach_terminal_commands::AttachCommandError,
        >(
            attach_terminal_commands::AttachReady {
                session_id: FAKE_SESSION,
                context_id: Some(FAKE_CONTEXT),
                can_write: true,
            },
        ))
        .unwrap()),
        "attach-set-viewport" => ok(bmux_plugin_sdk::encode_service_message(&Ok::<
            _,
            attach_terminal_commands::AttachCommandError,
        >(
            attach_terminal_commands::AttachViewportSet {
                session_id: FAKE_SESSION,
                cols: 80,
                rows: 24,
                top_inset: 0,
                right_inset: 0,
                bottom_inset: 0,
                left_inset: 0,
                context_id: Some(FAKE_CONTEXT),
            },
        ))
        .unwrap()),
        "attach-snapshot-state" => ok(bmux_plugin_sdk::encode_service_message(&Ok::<
            _,
            state::AttachStateError,
        >(
            state::AttachSnapshot {
                session_id: FAKE_SESSION,
                context_id: Some(FAKE_CONTEXT),
                focused_pane_id: FAKE_PANE,
                zoomed: false,
                layout_encoded: fake_layout_payload(None),
                chunks: Vec::new(),
                pane_mouse_protocols: Vec::new(),
                pane_input_modes: Vec::new(),
            },
        ))
        .unwrap()),
        "attach-layout-state" => ok(bmux_plugin_sdk::encode_service_message(&Ok::<
            _,
            state::AttachStateError,
        >(
            state::AttachLayout {
                session_id: FAKE_SESSION,
                context_id: Some(FAKE_CONTEXT),
                focused_pane_id: FAKE_PANE,
                encoded: fake_layout_payload(Some(false)),
            },
        ))
        .unwrap()),
        _ => Response::Err(ErrorResponse {
            code: bmux_ipc::ErrorCode::NotFound,
            message: format!("{operation}: not served by the fake server"),
        }),
    }
}

impl FakeAttachServer {
    /// A client connected to a new fake server.
    async fn connect(session: FakeSession) -> (bmux_client::BmuxClient, Self) {
        let (client_stream, server_stream) = tokio::io::duplex(1 << 20);
        let operations = Arc::new(Mutex::new(Vec::new()));
        let kill = Arc::new(tokio::sync::Notify::new());
        let (seen, killed) = (Arc::clone(&operations), Arc::clone(&kill));
        tokio::spawn(async move {
            let mut stream = ErasedIpcStream::new(Box::new(server_stream));
            loop {
                let envelope = tokio::select! {
                    () = killed.notified() => return,
                    envelope = stream.recv_envelope() => match envelope {
                        Ok(envelope) => envelope,
                        Err(_) => return,
                    },
                };
                let request: bmux_ipc::Request = bmux_ipc::decode(&envelope.payload).unwrap();
                let response = match request {
                    bmux_ipc::Request::Hello { contract, .. } => {
                        Response::Ok(ResponsePayload::HelloNegotiated {
                            negotiated: bmux_ipc::NegotiatedProtocol {
                                wire_epoch: contract.wire_epoch,
                                revision: contract.revisions.max,
                                capabilities: Vec::new(),
                            },
                        })
                    }
                    bmux_ipc::Request::SubscribeEvents => {
                        Response::Ok(ResponsePayload::EventsSubscribed)
                    }
                    bmux_ipc::Request::EnableEventPush => {
                        Response::Ok(ResponsePayload::EventPushEnabled)
                    }
                    bmux_ipc::Request::InvokeService { operation, .. } => {
                        seen.lock().unwrap().push(operation.clone());
                        fake_reply(&operation, session)
                    }
                    _ => Response::Err(ErrorResponse {
                        code: bmux_ipc::ErrorCode::NotFound,
                        message: "not served by the fake server".to_string(),
                    }),
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
            Duration::from_secs(2),
            "attach-reconnect-test",
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        (client, Self { operations, kill })
    }

    fn served(&self, operation: &str) -> usize {
        self.operations
            .lock()
            .unwrap()
            .iter()
            .filter(|served| *served == operation)
            .count()
    }
}

/// Wait (in real time) until `condition` holds.
async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn fast_reconnect_policy() -> super::super::reconnect::ReconnectPolicy {
    super::super::reconnect::ReconnectPolicy {
        max_attempts: None,
        backoff: super::super::reconnect::ReconnectBackoff::fixed(
            Duration::from_millis(20),
            Duration::from_millis(40),
        ),
        first_delay: None,
    }
}

/// The servers a reconnecting test attachment connects to, in order: each
/// connect takes the next (an empty slot refuses the connection).
type ServerSlots =
    Arc<Mutex<VecDeque<Option<(FakeSession, Arc<Mutex<Option<FakeAttachServer>>>)>>>>;

fn scripted_reconnect(slots: ServerSlots) -> super::super::reconnect::AttachReconnect {
    super::super::reconnect::AttachReconnect::new(move |_| {
        let slot = slots.lock().unwrap().pop_front().flatten();
        async move {
            let Some((session, server)) = slot else {
                anyhow::bail!("connection refused");
            };
            let (client, fake) = FakeAttachServer::connect(session).await;
            *server.lock().unwrap() = Some(fake);
            Ok(client.into())
        }
    })
    .with_policy(fast_reconnect_policy())
}

fn screen(handle: &HeadlessAttachTerminalHandle) -> String {
    handle.output_screen_text(80, 24)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // One attachment's whole outage, step by step.
async fn a_lost_connection_reconnects_in_place_and_rehydrates_the_view() {
    let (client, first) = FakeAttachServer::connect(FakeSession::Present).await;
    let second = Arc::new(Mutex::new(None::<FakeAttachServer>));
    let third = Arc::new(Mutex::new(None::<FakeAttachServer>));
    // The first reconnect attempt is refused; the second reaches a server.
    let slots: ServerSlots = Arc::new(Mutex::new(VecDeque::from([
        None,
        Some((FakeSession::Present, Arc::clone(&second))),
        Some((FakeSession::Present, Arc::clone(&third))),
    ])));
    let (inner, handle) = HeadlessAttachTerminal::new(80, 24);
    let restored = Arc::new(AtomicUsize::new(0));
    let mut terminal = CountingTerminal {
        inner,
        restored: Arc::clone(&restored),
    };
    let reconnect = scripted_reconnect(Arc::clone(&slots));
    let attach = tokio::spawn(async move {
        run_session_attach_with_terminal_reconnecting(
            client,
            Some(&FAKE_SESSION.to_string()),
            None,
            false,
            None,
            &mut terminal,
            Some(BmuxConfig::default()),
            None,
            None,
            Some(reconnect),
        )
        .await
    });

    eventually("the first hydrate", || {
        first.served("attach-snapshot-state") > 0
    })
    .await;
    first.kill.notify_one();
    eventually("the reconnecting banner", || {
        screen(&handle).contains("connection lost — reconnecting")
    })
    .await;
    assert_eq!(
        restored.load(Ordering::SeqCst),
        0,
        "attach mode kept while reconnecting"
    );
    // A key that is not the banner's is dropped (nothing to send it to).
    handle
        .send_event(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
        .unwrap();

    // Re-admitted on the new connection, then rebuilt from the server.
    eventually("the rehydrate on the new connection", || {
        second
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|server| server.served("attach-snapshot-state") > 0)
    })
    .await;
    for operation in [
        "current-client",
        "attach-context",
        "attach-open",
        "attach-set-viewport",
    ] {
        let served = second.lock().unwrap().as_ref().unwrap().served(operation);
        assert!(served > 0, "{operation} on the new connection");
    }
    eventually("the banner replaced by a full frame", || {
        !screen(&handle).contains("connection lost")
    })
    .await;
    assert!(!attach.is_finished(), "the attach loop keeps running");
    assert_eq!(restored.load(Ordering::SeqCst), 0);

    // Lost again: q while reconnecting leaves cleanly, restoring the terminal.
    second.lock().unwrap().as_ref().unwrap().kill.notify_one();
    eventually("the reconnecting banner again", || {
        screen(&handle).contains("connection lost — reconnecting")
    })
    .await;
    // Nothing to reconnect to this time.
    slots.lock().unwrap().clear();
    handle
        .send_event(Event::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        )))
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), attach)
        .await
        .expect("q ends the attachment")
        .unwrap()
        .expect("a clean exit");
    assert_eq!(outcome.exit_reason, AttachExitReason::Quit);
    assert_eq!(outcome.status_code, 0);
    assert!(
        restored.load(Ordering::SeqCst) >= 1,
        "the terminal is restored on exit"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_gone_after_reconnecting_ends_the_attachment_cleanly() {
    let (client, first) = FakeAttachServer::connect(FakeSession::Present).await;
    let second = Arc::new(Mutex::new(None::<FakeAttachServer>));
    let slots: ServerSlots = Arc::new(Mutex::new(VecDeque::from([Some((
        FakeSession::Gone,
        Arc::clone(&second),
    ))])));
    let (inner, _handle) = HeadlessAttachTerminal::new(80, 24);
    let restored = Arc::new(AtomicUsize::new(0));
    let mut terminal = CountingTerminal {
        inner,
        restored: Arc::clone(&restored),
    };
    let reconnect = scripted_reconnect(slots);
    let attach = tokio::spawn(async move {
        run_session_attach_with_terminal_reconnecting(
            client,
            Some(&FAKE_SESSION.to_string()),
            None,
            false,
            None,
            &mut terminal,
            Some(BmuxConfig::default()),
            None,
            None,
            Some(reconnect),
        )
        .await
    });
    eventually("the first hydrate", || {
        first.served("attach-snapshot-state") > 0
    })
    .await;
    first.kill.notify_one();
    let outcome = tokio::time::timeout(Duration::from_secs(10), attach)
        .await
        .expect("a gone session ends the attachment")
        .unwrap()
        .expect("a clean exit");
    assert_eq!(outcome.exit_reason, AttachExitReason::SessionGone);
    assert_eq!(outcome.status_code, 0);
    assert_eq!(
        attach_exit_message(outcome.exit_reason),
        Some("session no longer exists — attach ended")
    );
    assert!(restored.load(Ordering::SeqCst) >= 1);
}

// ── Native provider reopen ──────────────────────────────────────────────

/// A provider session that delivers scripted events, then waits.
#[derive(Debug)]
struct ScriptedProviderSession {
    events: VecDeque<Result<AttachProviderEvent, bmux_client::AttachSessionError>>,
}

impl ScriptedProviderSession {
    fn boxed(
        events: impl IntoIterator<Item = Result<AttachProviderEvent, bmux_client::AttachSessionError>>,
    ) -> Box<dyn AttachSession> {
        Box::new(Self {
            events: events.into_iter().collect(),
        })
    }
}

impl AttachSession for ScriptedProviderSession {
    fn initial_snapshot(
        &mut self,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderSnapshot> {
        Box::pin(async { Ok(empty_provider_snapshot()) })
    }

    fn next_event(&mut self) -> bmux_client::AttachSessionFuture<'_, AttachProviderEvent> {
        let event = self.events.pop_front();
        Box::pin(async move {
            match event {
                Some(event) => event,
                None => std::future::pending().await,
            }
        })
    }

    fn send_input(
        &mut self,
        _input: AttachProviderInput,
    ) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachProviderAck> {
        Box::pin(async { Err(bmux_client::AttachSessionError::Detached) })
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
        Box::pin(async { Err(bmux_client::AttachSessionError::Detached) })
    }

    fn detach(&mut self) -> bmux_client::AttachSessionFuture<'_, bmux_client::AttachDetachOutcome> {
        Box::pin(async { Ok(bmux_client::AttachDetachOutcome::Detached) })
    }
}

/// The resume states a provider was asked to reopen from, and when.
type ReopenRequests =
    Arc<Mutex<Vec<(Option<bmux_client::AttachResumeState>, tokio::time::Instant)>>>;

/// Reopens with each scripted session in turn, recording the resume state
/// it was asked to resume from and when.
fn scripted_reopen(
    sessions: Vec<Box<dyn AttachSession>>,
    asked: ReopenRequests,
) -> super::super::reconnect::ProviderReopen {
    let sessions = Arc::new(Mutex::new(VecDeque::from(sessions)));
    super::super::reconnect::ProviderReopen::new(move |resume| {
        asked
            .lock()
            .unwrap()
            .push((resume, tokio::time::Instant::now()));
        let session = sessions.lock().unwrap().pop_front();
        async move { session.ok_or_else(|| anyhow::anyhow!("provider unreachable")) }
    })
    .with_policy(fast_reconnect_policy())
}

#[tokio::test(start_paused = true)]
async fn a_recoverable_provider_disconnect_reopens_after_the_wait_it_asked_for() {
    let (inner, _handle) = HeadlessAttachTerminal::new(80, 24);
    let restored = Arc::new(AtomicUsize::new(0));
    let mut terminal = CountingTerminal {
        inner,
        restored: Arc::clone(&restored),
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let started = tokio::time::Instant::now();
    let outcome = run_native_attach_session_with_terminal(
        ScriptedProviderSession::boxed([Ok(AttachProviderEvent::Disconnected(
            bmux_client::AttachProviderDisconnect {
                recoverable: true,
                reason: "relay restarting".into(),
                resume: None,
                retry_after_ms: Some(750),
            },
        ))]),
        &mut terminal,
        Some(scripted_reopen(
            vec![ScriptedProviderSession::boxed([Ok(
                AttachProviderEvent::Detached,
            )])],
            Arc::clone(&asked),
        )),
    )
    .await
    .expect("the reopened provider detached cleanly");
    assert_eq!(outcome.exit_reason, AttachExitReason::Detached);
    let asked = std::mem::take(&mut *asked.lock().unwrap());
    assert_eq!(asked.len(), 1);
    let (resume, at) = &asked[0];
    assert!(resume.is_some(), "resumes from the validated view");
    assert!(
        *at - started >= Duration::from_millis(750),
        "waited as asked"
    );
    assert_eq!(
        restored.load(Ordering::SeqCst),
        1,
        "restored once, at the end"
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_transport_loss_resumes_and_a_continuity_break_starts_fresh() {
    let (inner, _handle) = HeadlessAttachTerminal::new(80, 24);
    let mut terminal = CountingTerminal {
        inner,
        restored: Arc::new(AtomicUsize::new(0)),
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let outcome = run_native_attach_session_with_terminal(
        ScriptedProviderSession::boxed([Err(bmux_client::AttachSessionError::Provider {
            reason: "transport closed".into(),
        })]),
        &mut terminal,
        Some(scripted_reopen(
            vec![
                ScriptedProviderSession::boxed([Err(
                    bmux_client::AttachSessionError::SequenceGap {
                        expected: 2,
                        actual: 9,
                    },
                )]),
                ScriptedProviderSession::boxed([Ok(AttachProviderEvent::Detached)]),
            ],
            Arc::clone(&asked),
        )),
    )
    .await
    .expect("reopened twice, then detached");
    assert_eq!(outcome.exit_reason, AttachExitReason::Detached);
    let resumes: Vec<bool> = asked
        .lock()
        .unwrap()
        .iter()
        .map(|(resume, _)| resume.is_some())
        .collect();
    assert_eq!(resumes, [true, false]);
}
