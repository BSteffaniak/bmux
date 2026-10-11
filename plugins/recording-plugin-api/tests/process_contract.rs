use bmux_recording_plugin_api::recording_process_v4::{self as process, RunPhase};

#[test]
fn process_contract_status_keeps_exit_distinct_from_capture() {
    assert_eq!(process::INTERFACE_ID, "recording-process-v4");
    let value = process::RunStatus {
        schema_version: 4,
        run_id: uuid::Uuid::new_v4(),
        revision: 1,
        launch_mutation_id: uuid::Uuid::new_v4(),
        recording_id: uuid::Uuid::new_v4(),
        execution: None,
        runtime_root: "/isolated/runtime".into(),
        phase: RunPhase::Exited,
        exit_code: Some(7),
        target_exited: true,
        terminal_session_ended: false,
        capture_eof: false,
        capture_watermark: 0,
        recording_watermark: 0,
        durable_recording_complete: false,
        timing_complete: true,
        cleanup_complete: false,
        recovery_required: false,
        output_drained: false,
        output_watermark: 0,
        capture_finalized: false,
        errors: vec![],
    };
    let json = serde_json::to_value(&value).unwrap();
    assert_eq!(json["phase"], "exited");
    assert_eq!(json["exit_code"], 7);
    assert_eq!(json["capture_finalized"], false);
    let decoded: process::RunStatus = serde_json::from_value(json).unwrap();
    assert_eq!(decoded.run_id, value.run_id);
}
