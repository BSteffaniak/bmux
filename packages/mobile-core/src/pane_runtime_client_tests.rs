//! Tests exercise the actual generated transport requests, not handwritten envelopes.
use super::*;
use bmux_plugin_sdk::{TypedDispatchClient, TypedDispatchClientError, TypedDispatchClientResult};

struct GridDispatch {
    pane_id: Uuid,
    reply: Vec<u8>,
    reject: bool,
    calls: usize,
}

impl TypedDispatchClient for GridDispatch {
    fn invoke_service_raw(
        &mut self,
        capability: &str,
        kind: bmux_ipc::InvokeServiceKind,
        interface: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl std::future::Future<Output = TypedDispatchClientResult<Vec<u8>>> {
        let mut payload = Some(payload);
        std::future::poll_fn(move |_| {
            std::task::Poll::Ready({
                let payload = payload.take().expect("transport polled after completion");
                self.calls += 1;
                assert_eq!(capability, "bmux.pane_runtime.pointer_grid.v1");
                assert_eq!(interface, "pane-runtime-pointer-grid-v2");
                assert!(matches!(kind, bmux_ipc::InvokeServiceKind::Command));
                match operation {
                    "snapshots" => {
                        let request: pointer_grid::client::SnapshotsRequest =
                            bmux_plugin_sdk::decode_service_message(&payload).unwrap();
                        assert_eq!(request.format_version, 1);
                        assert_eq!(request.pane_ids, vec![self.pane_id]);
                        assert_eq!(request.max_rows_per_pane, 2);
                    }
                    "frames" => {
                        let request: pointer_grid::client::FramesRequest =
                            bmux_plugin_sdk::decode_service_message(&payload).unwrap();
                        assert_eq!(request.format_version, 1);
                        assert_eq!(request.pane_ids, vec![self.pane_id]);
                        assert_eq!(request.max_history_lines, 4);
                    }
                    _ => panic!("unexpected legacy fallback: {operation}"),
                }
                if self.reject {
                    Err(TypedDispatchClientError::server(
                        interface,
                        operation,
                        "unsupported_representation",
                    ))
                } else {
                    Ok(self.reply.clone())
                }
            })
        })
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

#[test]
fn generated_snapshot_transport_preserves_complete_pointer_recovery_state() {
    use bmux_terminal_grid::{GridLimits, TerminalGridStream};
    let pane_id = Uuid::new_v4();
    let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
    producer.process(b"\x1b]22;pointer\x07\x1b]22;>crosshair\x07\x1b]22;>text");
    let snapshot = producer.snapshot(0, 2);
    let response = Ok::<_, attach_state::AttachStateError>(attach_state::AttachPaneGridSnapshot {
        snapshots: vec![attach_state::PaneGridSnapshot {
            pane_id,
            stream_end: 100,
            encoded: serde_json::to_vec(&snapshot).unwrap(),
        }],
    });
    let mut dispatch = GridDispatch {
        pane_id,
        reply: bmux_plugin_sdk::encode_service_message(&response).unwrap(),
        reject: false,
        calls: 0,
    };
    let received = runtime()
        .block_on(attach_pane_grid_snapshot_state_streaming(
            &mut dispatch,
            Uuid::nil(),
            vec![pane_id],
            2,
        ))
        .unwrap();
    let decoded: bmux_terminal_grid::GridSnapshot =
        serde_json::from_slice(&received[0].encoded).unwrap();
    assert_eq!(decoded, snapshot);
    assert_eq!(received[0].stream_end, 100);
    assert_eq!(dispatch.calls, 1);
}

#[test]
fn generated_frame_transport_carries_frames_that_apply() {
    use bmux_terminal_grid::{GridLimits, ReplicaCursor, TerminalGrid, TerminalGridStream};
    let pane_id = Uuid::new_v4();
    let mut producer = TerminalGridStream::new(10, 2, GridLimits::default()).unwrap();
    producer.process(b"hi\x1b]22;pointer\x07");
    let mut frame = Vec::new();
    producer
        .grid_mut()
        .write_frame_since(ReplicaCursor::default(), usize::MAX, &mut frame);
    let response = Ok::<_, attach_state::AttachStateError>(attach_state::AttachPaneFrames {
        frames: vec![attach_state::PaneFrame {
            pane_id,
            sync_update_active: true,
            encoded: frame.clone(),
            replay_markers: Vec::new(),
            clipboard_writes: Vec::new(),
        }],
        frames_still_pending: false,
    });
    let mut dispatch = GridDispatch {
        pane_id,
        reply: bmux_plugin_sdk::encode_service_message(&response).unwrap(),
        reject: false,
        calls: 0,
    };
    let (received, pending) = runtime()
        .block_on(attach_pane_frames_streaming(
            &mut dispatch,
            Uuid::nil(),
            vec![pane_id],
            4,
        ))
        .unwrap();
    assert!(!pending);
    assert!(received[0].sync_update_active);
    assert_eq!(received[0].encoded, frame);
    let mut replica = TerminalGrid::new(1, 1, GridLimits::default()).unwrap();
    replica
        .apply_frame(&received[0].encoded, &mut ReplicaCursor::default())
        .unwrap();
    assert_eq!(
        replica.snapshot(0, 2).rows,
        producer.grid().snapshot(0, 2).rows
    );
    assert_eq!(dispatch.calls, 1);
}

#[test]
fn unsupported_pointer_representation_does_not_retry_legacy_transport() {
    let pane_id = Uuid::new_v4();
    let mut dispatch = GridDispatch {
        pane_id,
        reply: Vec::new(),
        reject: true,
        calls: 0,
    };
    let runtime = runtime();
    assert!(
        runtime
            .block_on(attach_pane_grid_snapshot_state_streaming(
                &mut dispatch,
                Uuid::nil(),
                vec![pane_id],
                2,
            ))
            .is_err()
    );
    assert_eq!(dispatch.calls, 1);
    assert!(
        runtime
            .block_on(attach_pane_frames_streaming(
                &mut dispatch,
                Uuid::nil(),
                vec![pane_id],
                4,
            ))
            .is_err()
    );
    assert_eq!(dispatch.calls, 2);
}
