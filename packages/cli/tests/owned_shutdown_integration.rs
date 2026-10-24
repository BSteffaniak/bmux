//! Real-process acceptance of one-command, instance-bound shutdown.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;

use bmux_pane_runtime_plugin_api::pane_runtime_commands::client;
use std::time::Duration;

#[tokio::test]
async fn one_stop_completes_all_runtimes_and_publishes_matching_receipt() {
    let mut server = support::ServerEnv::new("owned-shutdown");
    server.write_config(
        "[general]\ndefault_shell = '/bin/sh'\n[behavior]\nrestore_last_layout = true\n",
    );
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut connection =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(10), "shutdown-test")
            .await
            .unwrap();
    for index in 0..3 {
        client::new_session_with_runtime(&mut connection, Some(format!("shutdown-{index}")))
            .await
            .unwrap()
            .unwrap();
    }
    drop(connection);
    let pid = server.server_pid().unwrap();
    server.stop();
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(server.runtime_dir.join("shutdown-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["pid"], pid);
    assert!(receipt["started_at_epoch_ms"].as_u64().unwrap() > 0);
    assert!(receipt["error"].is_null(), "cleanup failed: {receipt}");
    let logs = server.server_output();
    assert!(logs.contains("shutdown completed"), "{logs}");
    server.start();
    server.stop();
}
