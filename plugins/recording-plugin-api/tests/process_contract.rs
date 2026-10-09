use bmux_recording_plugin_api::recording_process_v1::{self as process, RunPhase};

#[test]
fn process_contract_status_keeps_exit_distinct_from_capture() {
    assert_eq!(process::INTERFACE_ID, "recording-process-v1");
    let value = process::RunStatus {
        schema_version: 1,
        run_id: uuid::Uuid::new_v4(),
        revision: 1,
        recording_id: uuid::Uuid::new_v4(),
        execution: None,
        runtime_root: "/isolated/runtime".into(),
        phase: RunPhase::Exited,
        exit_code: Some(7),
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
