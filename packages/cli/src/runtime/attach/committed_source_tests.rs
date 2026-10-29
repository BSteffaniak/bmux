//! Existing authenticated connection; committed-only, refresh-aware complete frames.
use super::*;
use bmux_ipc::transport::ErasedIpcStream;
use bmux_ipc::{Envelope, EnvelopeKind, Response, ResponsePayload};

#[tokio::test]
#[allow(clippy::too_many_lines)] // Transcript proves connection identity and complete refresh replay together.
async fn same_connection_publisher_resends_full_committed_frame_only_on_refresh() {
    use bmux_broadcast_plugin_api::broadcast_output_v1 as api;
    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        let mut stream = ErasedIpcStream::new(Box::new(server_stream));
        let mut polls = 0;
        let mut published = Vec::new();
        // hello, poll(no commit), poll+publish, poll(unchanged), poll+publish(refresh)
        for step in 0..7 {
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
                        capabilities: vec![],
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
                    panic!("same service connection")
                };
                assert_eq!(interface_id, "broadcast-output-v1");
                let payload = payload.as_slice();
                let encoded = if operation == "poll" {
                    polls += 1;
                    let request: api::client::PollRequest =
                        bmux_plugin_sdk::decode_service_message(payload).unwrap();
                    assert_eq!(request.source_id, "owned-view");
                    bmux_plugin_sdk::encode_service_message(&api::SourceState {
                        refresh: if polls < 4 { 1 } else { 2 },
                        selected: true,
                    })
                    .unwrap()
                } else {
                    assert_eq!(operation, "publish");
                    let request: api::client::PublishRequest =
                        bmux_plugin_sdk::decode_service_message(payload).unwrap();
                    assert_eq!(request.source_id, "owned-view");
                    let frame = bmux_web_renderer::decode_binary(&request.frame, None).unwrap();
                    assert_eq!(frame.revision(), 7);
                    published.push((request.refresh, request.frame));
                    bmux_plugin_sdk::encode_service_message(&true).unwrap()
                };
                Response::Ok(ResponsePayload::ServiceInvoked { payload: encoded })
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
        published
    });
    let connection = bmux_client::BmuxClient::connect_with_bridge_stream(
        ErasedIpcStream::new(Box::new(client_stream)),
        std::time::Duration::from_secs(2),
        "source-test",
        uuid::Uuid::new_v4(),
    )
    .await
    .unwrap();
    let mut connection = StreamingBmuxClient::from_client(connection).unwrap();
    let presentation =
        bmux_tui::presentation::PresentationState::new(bmux_tui::geometry::Rect::new(0, 0, 2, 1));
    let prepared = presentation
        .prepare(bmux_tui::damage::Damage::Full, |_| {})
        .unwrap();
    let snapshot = AttachSnapshot {
        epoch: uuid::Uuid::new_v4(),
        revision: 7,
        attached_id: uuid::Uuid::new_v4(),
        prepared: prepared.into(),
        surfaces: vec![],
        selection_text: std::collections::BTreeMap::new(),
        unsupported_metadata: &[],
    };
    let mut source = CommittedSource {
        id: "owned-view".into(),
        sent: None,
    };
    source.poll(&mut connection, None).await;
    source.poll(&mut connection, Some(&snapshot)).await;
    source.poll(&mut connection, Some(&snapshot)).await;
    source.poll(&mut connection, Some(&snapshot)).await;
    let published = server.await.unwrap();
    assert_eq!(published.len(), 2);
    assert_eq!(published[0].0, 1);
    assert_eq!(published[1].0, 2);
    assert_eq!(published[0].1, published[1].1);
}
