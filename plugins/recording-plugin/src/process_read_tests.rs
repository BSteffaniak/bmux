#[cfg(test)]
mod tests {
    use super::*;
    use bmux_recording_protocol::{
        RecordingEventEnvelope, RecordingEventKind, RecordingPayload, write_frame,
    };
    #[test]
    fn byte_cursor_is_bounded_and_preserves_binary_output() {
        let root = std::env::temp_dir().join(format!("bmux-process-read-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let session_id = Uuid::new_v4();
        let pane_id = Uuid::new_v4();
        let value = process::RunStatus {
            schema_version: 1,
            run_id: Uuid::new_v4(),
            revision: 1,
            recording_id: Uuid::new_v4(),
            execution: Some(process::ExecutionRef {
                session_id,
                pane_id,
                execution_token: Uuid::new_v4(),
            }),
            runtime_root: "/isolated/runtime".into(),
            phase: process::RunPhase::Finalized,
            exit_code: Some(0),
            output_drained: true,
            output_watermark: 4,
            capture_finalized: true,
            errors: vec![],
        };
        let event: RecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode> =
            RecordingEventEnvelope {
                seq: 1,
                mono_ns: 0,
                wall_epoch_ms: 0,
                session_id: Some(session_id),
                pane_id: Some(pane_id),
                client_id: None,
                kind: RecordingEventKind::PaneOutputRaw,
                payload: RecordingPayload::Bytes {
                    data: vec![0, 255, 1, 2],
                },
            };
        let mut file = std::fs::File::create(root.join("events_0.bin")).unwrap();
        write_frame(&mut file, &event).unwrap();
        std::fs::write(root.join("output.live"), [0, 255, 1, 2]).unwrap();
        let first = read_output(&root, &value, 0, 2).unwrap();
        assert_eq!(first.data, [0, 255]);
        assert_eq!(first.next_cursor, 2);
        let next = read_output(&root, &value, first.next_cursor, 8).unwrap();
        assert_eq!(next.data, [1, 2]);
        assert!(next.complete);
        assert!(read_output(&root, &value, 5, 8).is_err());
        assert!(read_output(&root, &value, 0, 0).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
