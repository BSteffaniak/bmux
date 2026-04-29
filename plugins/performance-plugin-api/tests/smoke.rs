use bmux_performance_plugin_api::{capabilities, performance_commands, performance_state};

#[test]
fn interface_ids_match_bpdl_source() {
    assert_eq!(performance_state::INTERFACE_ID, "performance-state");
    assert_eq!(performance_commands::INTERFACE_ID, "performance-commands");
    assert_eq!(capabilities::PERFORMANCE_READ, "bmux.performance.read");
    assert_eq!(capabilities::PERFORMANCE_WRITE, "bmux.performance.write");
}

#[test]
fn runtime_settings_convert_to_ipc() {
    let settings = bmux_performance_plugin_api::performance_types::PerformanceRuntimeSettings {
        recording_level:
            bmux_performance_plugin_api::performance_types::PerformanceRecordingLevel::Detailed,
        window_ms: 500,
        max_events_per_sec: 10,
        max_payload_bytes_per_sec: 2048,
    };

    let ipc: bmux_ipc::PerformanceRuntimeSettings = settings.into();
    assert_eq!(
        ipc.recording_level,
        bmux_ipc::PerformanceRecordingLevel::Detailed
    );
    assert_eq!(ipc.max_payload_bytes_per_sec, 2048);
}
