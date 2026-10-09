#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immediate_output_replays_without_polling_and_retains_raw_bytes() {
        let root = std::env::temp_dir().join(format!("bmux-replay-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let bytes = b"\x1b[2J\x1b[H\x1b[41mRED\x1b[0m".to_vec();
        let value: RecordingEventEnvelope<bmux_ipc::Event, bmux_ipc::ErrorCode> =
            RecordingEventEnvelope {
                seq: 1,
                mono_ns: 100,
                wall_epoch_ms: 0,
                session_id: None,
                pane_id: None,
                client_id: None,
                kind: RecordingEventKind::PaneOutputRaw,
                payload: RecordingPayload::Bytes {
                    data: bytes.clone(),
                },
            };
        let mut file = std::fs::File::create(root.join("events_0.bin")).unwrap();
        write_frame(&mut file, &value).unwrap();
        let id = Uuid::new_v4();
        std::fs::write(root.join("manifest.json"),serde_json::to_vec(&serde_json::json!({"summary": {
            "id":id,"capture_input":false,"started_epoch_ms":0,"ended_epoch_ms":1,"event_count":1,"payload_bytes":bytes.len(),"path":root.to_string_lossy(),"segments":["events_0.bin"],"total_segment_bytes":0
        }})).unwrap()).unwrap();
        let screen = replay(&root, id, 80, 24).unwrap();
        assert!(screen.contains("RED"));
        assert_eq!(std::fs::read(root.join("output.live")).unwrap(), bytes);
        let frames = std::fs::read(root.join(format!("display-{}.bin", Uuid::nil()))).unwrap();
        let track = bmux_recording_protocol::read_frames::<DisplayTrackEnvelope>(&frames).unwrap();
        assert_eq!(
            track
                .frames
                .iter()
                .find(|event| matches!(event.event, DisplayTrackEvent::FrameBytes { .. }))
                .unwrap()
                .mono_ns,
            0,
            "immediate output must be drawable at the first export sample"
        );
        assert!(
            track
                .frames
                .iter()
                .any(|event| matches!(event.event, DisplayTrackEvent::FrameBytes { .. }))
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
