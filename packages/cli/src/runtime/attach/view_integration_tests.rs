//! Caller-bound layout transport and private retained-bus regression tests.
use super::*;
use bmux_ipc::transport::ErasedIpcStream;
use bmux_ipc::{Envelope, EnvelopeKind, Response, ResponsePayload};

#[tokio::test]
#[allow(clippy::too_many_lines)] // Keep the transport transcript and its lifetime assertions together.
async fn scoped_layouts_have_private_buses_focus_and_monotonic_revisions() {
    use super::attach_decoration_view_api as api;
    let (client_stream, server_stream) = tokio::io::duplex(16384);
    let server = tokio::spawn(async move {
        let mut stream = ErasedIpcStream::new(Box::new(server_stream));
        let mut requests = Vec::new();
        for step in 0..4 {
            let envelope = stream.recv_envelope().await.unwrap();
            let request: bmux_ipc::Request = bmux_ipc::decode(&envelope.payload).unwrap();
            let response = if step == 0 {
                let bmux_ipc::Request::Hello { contract, .. } = request else {
                    panic!("hello")
                };
                Response::Ok(ResponsePayload::HelloNegotiated {
                    negotiated: bmux_ipc::NegotiatedProtocol {
                        wire_epoch: contract.wire_epoch,
                        revision: contract.revisions.max,
                        capabilities: Vec::new(),
                    },
                })
            } else {
                let bmux_ipc::Request::InvokeService {
                    interface_id,
                    operation,
                    payload,
                    ..
                } = request
                else {
                    panic!("typed service")
                };
                assert_eq!(interface_id, "decoration-view-v1");
                assert_eq!(operation, "update-layout");
                let payload = payload.as_slice();
                requests.push(
                    bmux_plugin_sdk::decode_service_message::<api::client::UpdateLayoutRequest>(
                        payload,
                    )
                    .unwrap(),
                );
                Response::Ok(ResponsePayload::ServiceInvoked {
                    payload: bmux_plugin_sdk::encode_service_message(&Ok::<(), String>(()))
                        .unwrap(),
                })
            };
            stream
                .send_envelope(&Envelope::new(
                    envelope.request_id,
                    EnvelopeKind::Response,
                    bmux_ipc::encode(&response).unwrap(),
                ))
                .await
                .unwrap();
        }
        requests
    });
    let client = bmux_client::BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(client_stream)),
        Duration::from_secs(2),
        "view-test",
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    let mut client = StreamingBmuxClient::from_client(client).unwrap();
    let create = || {
        let state = AttachViewState::with_private_presentation(bmux_client::AttachOpenInfo {
            context_id: None,
            session_id: Uuid::new_v4(),
            can_write: true,
        });
        state.presentation_events.register_state_channel(
            ATTACH_LAYOUT_STATE_KIND,
            AttachLayoutSnapshot {
                surfaces: vec![],
                revision: 0,
            },
        );
        state
    };
    let mut first = create();
    let mut second = create();
    let (_, first_rx) = first
        .presentation_events
        .subscribe_state::<AttachLayoutSnapshot>(&ATTACH_LAYOUT_STATE_KIND)
        .unwrap();
    let (_, second_rx) = second
        .presentation_events
        .subscribe_state::<AttachLayoutSnapshot>(&ATTACH_LAYOUT_STATE_KIND)
        .unwrap();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    publish_attach_layout_snapshot(&mut client, &mut first, &[], Some(a)).await;
    assert!(first_rx.has_changed().unwrap());
    assert!(!second_rx.has_changed().unwrap());
    publish_attach_layout_snapshot(&mut client, &mut second, &[], Some(b)).await;
    // A replacement target cannot rewind this connection's layout baseline.
    first.attached_id = Uuid::new_v4();
    publish_attach_layout_snapshot(&mut client, &mut first, &[], Some(b)).await;
    let requests = server.await.unwrap();
    assert_ne!(requests[0].lifetime, requests[1].lifetime);
    assert_eq!(requests[0].lifetime, requests[2].lifetime);
    assert_eq!(
        requests.iter().map(|r| r.revision).collect::<Vec<_>>(),
        vec![1, 1, 2]
    );
    assert_eq!(requests[0].focused_pane_id, Some(a));
    assert_eq!(requests[1].focused_pane_id, Some(b));
    assert_eq!(requests[2].focused_pane_id, Some(b));
}

#[test]
fn scene_baseline_fences_old_lifetimes_and_reconnect_has_fresh_state() {
    let mut baseline = 0;
    assert!(accept_scene_revision(&mut baseline, 40));
    assert!(!accept_scene_revision(&mut baseline, 39));
    assert!(!accept_scene_revision(&mut baseline, 40));
    // Close's empty barrier is newer; queued old-target output cannot revive it.
    assert!(accept_scene_revision(&mut baseline, 41));
    assert!(!accept_scene_revision(&mut baseline, 40));
    assert!(accept_scene_revision(&mut baseline, 42));
    let mut reconnected_baseline = 0;
    assert!(accept_scene_revision(&mut reconnected_baseline, 1));
    assert_eq!(baseline, 42);
}
