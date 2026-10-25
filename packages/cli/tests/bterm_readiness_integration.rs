//! Fresh-server readiness registration must work before any terminal is spawned.
#[allow(dead_code)] // Shared fixture also supports gateway and log-inspection tests.
mod support;

#[tokio::test]
async fn fresh_companion_exposes_authoritative_readiness() {
    let mut server = support::ServerEnv::new("bterm-readiness");
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut client = bmux_client::BmuxClient::connect(
        &endpoint,
        std::time::Duration::from_secs(5),
        "bterm-readiness-test",
    )
    .await
    .unwrap();
    client.require_service_contracts().unwrap();
    let readiness =
        bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client::runtime_readiness(
            &mut client,
        )
        .await
        .expect("fresh companion must register readiness service");
    assert_eq!(readiness.runtime_dir, server.runtime_dir.to_string_lossy());
    assert!(!readiness.fenced);
    assert!(
        readiness.managed_target_identity.is_none(),
        "ordinary test server is not a managed bundle launch"
    );
    #[cfg(target_os = "linux")]
    assert!(
        readiness.executable_sha256.is_some(),
        "Linux has mapped-image proof through /proc/self/exe"
    );
    #[cfg(not(target_os = "linux"))]
    assert!(
        readiness.executable_sha256.is_none(),
        "ordinary server has no immutable-bundle startup attestation; do not report a mutable pathname hash as mapped-image proof"
    );
    assert!(readiness.helper_ready || readiness.helper_error.is_some());
    server.stop();
}
