use bmux_recording_plugin_api::{
    capabilities, recording_commands, recording_events, recording_state, recording_types,
};

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(recording_state::INTERFACE_ID, "recording-state");
    assert_eq!(recording_commands::INTERFACE_ID, "recording-commands");
    assert_eq!(capabilities::RECORDING_READ, "bmux.recording.read");
    assert_eq!(capabilities::RECORDING_WRITE, "bmux.recording.write");
}

#[test]
fn recording_job_event_serializes() {
    let job = recording_types::RecordingJob {
        id: uuid::Uuid::nil(),
        kind: recording_types::RecordingJobKind::Cut,
        status: recording_types::RecordingJobStatus::Exporting,
        recording_id: Some(uuid::Uuid::nil()),
        recording_path: Some("/tmp/raw".to_string()),
        export_output_path: Some("/tmp/out.gif".to_string()),
        error: None,
    };
    let event = recording_events::RecordingEvent::JobUpdated { job };

    let json = serde_json::to_string(&event).expect("job event should serialize");
    assert!(json.contains("job_updated"));
    assert!(json.contains("exporting"));
}

#[test]
fn recording_summary_converts_to_protocol() {
    let summary = recording_types::RecordingSummary {
        id: uuid::Uuid::nil(),
        name: Some("demo".to_string()),
        format_version: 6,
        session_id: None,
        capture_input: false,
        profile: recording_types::RecordingProfile::Functional,
        event_kinds: vec![recording_types::RecordingEventKind::PaneOutputRaw],
        started_epoch_ms: 1,
        ended_epoch_ms: None,
        event_count: 2,
        payload_bytes: 3,
        path: "/tmp/demo".to_string(),
        segments: vec!["events_0.bin".to_string()],
        total_segment_bytes: 4,
    };

    let protocol: bmux_recording_protocol::RecordingSummary = summary.into();
    assert_eq!(
        protocol.profile,
        bmux_recording_protocol::RecordingProfile::Functional
    );
    assert_eq!(
        protocol.event_kinds,
        vec![bmux_recording_protocol::RecordingEventKind::PaneOutputRaw]
    );
}
