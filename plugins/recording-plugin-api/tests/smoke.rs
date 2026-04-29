use bmux_recording_plugin_api::{
    capabilities, recording_commands, recording_state, recording_types,
};

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(recording_state::INTERFACE_ID, "recording-state");
    assert_eq!(recording_commands::INTERFACE_ID, "recording-commands");
    assert_eq!(capabilities::RECORDING_READ, "bmux.recording.read");
    assert_eq!(capabilities::RECORDING_WRITE, "bmux.recording.write");
}

#[test]
fn recording_summary_converts_to_ipc() {
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

    let ipc: bmux_ipc::RecordingSummary = summary.into();
    assert_eq!(ipc.profile, bmux_ipc::RecordingProfile::Functional);
    assert_eq!(
        ipc.event_kinds,
        vec![bmux_ipc::RecordingEventKind::PaneOutputRaw]
    );
}
