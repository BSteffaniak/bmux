use super::super::server_runtime::{
    ProcessObservation, ProcessWaitDecision, process_wait_decision,
};
use super::*;

fn metadata() -> ServerRuntimeMetadata {
    ServerRuntimeMetadata {
        pid: 42,
        version: String::new(),
        build_id: String::new(),
        executable_path: String::new(),
        started_at_epoch_ms: 100,
        slot_name: None,
    }
}

#[test]
fn completion_requires_both_identity_fields() {
    let metadata = metadata();
    for (pid, started_at_epoch_ms, expected) in [
        (42, 100, CleanupProof::Complete),
        (43, 100, CleanupProof::Unavailable),
        (42, 101, CleanupProof::Unavailable),
    ] {
        let receipt = ShutdownReceipt {
            pid,
            started_at_epoch_ms,
            error: None,
        };
        assert_eq!(cleanup_proof(Some(&metadata), Some(&receipt)), expected);
    }
}

#[test]
fn missing_legacy_record_or_metadata_never_proves_cleanup() {
    let receipt = ShutdownReceipt {
        pid: 42,
        started_at_epoch_ms: 100,
        error: None,
    };
    assert_eq!(
        cleanup_proof(Some(&metadata()), None),
        CleanupProof::Unavailable
    );
    assert_eq!(
        cleanup_proof(None, Some(&receipt)),
        CleanupProof::Unavailable
    );
    assert_eq!(cleanup_proof(None, None), CleanupProof::Unavailable);
    assert_eq!(report_local_stop(CleanupProof::Unavailable).unwrap(), 0);
}

#[test]
fn cleanup_failure_is_not_reported_as_success() {
    let receipt = ShutdownReceipt {
        pid: 42,
        started_at_epoch_ms: 100,
        error: Some("owned cleanup failed".into()),
    };
    let proof = cleanup_proof(Some(&metadata()), Some(&receipt));
    assert_eq!(proof, CleanupProof::Failed("owned cleanup failed".into()));
    assert!(
        report_local_stop(proof)
            .unwrap_err()
            .to_string()
            .contains("owned cleanup failed")
    );
}

#[test]
fn stale_failure_does_not_contaminate_another_generation() {
    let receipt = ShutdownReceipt {
        pid: 42,
        started_at_epoch_ms: 99,
        error: Some("previous generation".into()),
    };
    assert_eq!(
        cleanup_proof(Some(&metadata()), Some(&receipt)),
        CleanupProof::Unavailable
    );
}

#[test]
fn corrupt_receipt_is_an_error() {
    assert!(serde_json::from_slice::<ShutdownReceipt>(b"not JSON").is_err());
    assert!(serde_json::from_slice::<ShutdownReceipt>(br#"{"pid":42}"#).is_err());
}

#[test]
fn deadline_decisions_never_infer_exit_from_elapsed_time() {
    assert_eq!(
        process_wait_decision(false, false),
        ProcessWaitDecision::Wait
    );
    assert_eq!(
        process_wait_decision(false, true),
        ProcessWaitDecision::TimedOut
    );
    assert_eq!(
        process_wait_decision(true, false),
        ProcessWaitDecision::Exited
    );
    assert_eq!(
        process_wait_decision(true, true),
        ProcessWaitDecision::Exited
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn live_process_at_expired_deadline_is_not_success() {
    let process = ProcessObservation::open(std::process::id()).unwrap();
    assert!(!process.exited().unwrap());
    assert!(!process.wait(tokio::time::Instant::now()).await.unwrap());
    assert!(!process.exited().unwrap());
}

#[test]
fn invalid_pid_is_rejected_without_signaling() {
    assert!(ProcessObservation::open(0).is_err());
}

#[test]
fn timeout_recovery_message_does_not_offer_unsafe_pid_kill() {
    assert_eq!(OWNED_STOP_TIMEOUT, std::time::Duration::from_secs(30));
    assert!(STOP_TIMEOUT_MESSAGE.contains("no termination signal was sent"));
    assert!(STOP_TIMEOUT_MESSAGE.contains("retry `server stop`"));
    assert!(STOP_TIMEOUT_MESSAGE.contains("stale PID file"));
}

#[test]
fn remote_transport_loss_is_not_successful_completion() {
    let error = report_remote_completion().unwrap_err().to_string();
    assert!(error.contains("cannot be proved"));
    assert!(error.contains("no shutdown completion record"));
    assert!(error.contains("no local fallback"));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn owned_process_exit_remains_observed_after_reaping() {
    // Run only the test harness help, not a server or user runtime.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let observation = ProcessObservation::open(child.id()).unwrap();
    assert!(child.wait().unwrap().success());
    assert!(
        observation
            .wait(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
            .await
            .unwrap()
    );
    // kqueue events may be consumed, but completion must remain latched.
    assert!(observation.exited().unwrap());
    assert!(observation.exited().unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_observation_retains_owned_kqueue() {
    let observation = ProcessObservation::open(std::process::id()).unwrap();
    assert!(!observation.exited().unwrap());
    assert!(!observation.exited().unwrap());
}

#[cfg(windows)]
#[test]
fn windows_observation_retains_owned_process_handle() {
    let observation = ProcessObservation::open(std::process::id()).unwrap();
    assert!(!observation.exited().unwrap());
    assert!(!observation.exited().unwrap());
}
