//! Fresh-server readiness registration must work before any terminal is spawned.
#[allow(dead_code)] // Shared fixture also supports gateway and log-inspection tests.
mod support;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_companion_idle_watch_does_not_block_handshake_or_lose_host_origin() {
    use bmux_pane_runtime_plugin_api::pane_runtime_readiness_v1::client;
    use std::time::{Duration, Instant};
    let server = support::ServerEnv::new("bterm-managed-readiness");
    let marker = r#"{"selected":"managed-readiness-test"}"#;
    let log = server.root().join("managed-startup.log");
    let output = std::fs::File::create(&log).unwrap();
    let server_output = || std::fs::read_to_string(&log).unwrap_or_default();
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = OwnedChild(
        server
            .command(&["server", "start", "--foreground-internal"])
            .env("BMUX_MANAGED_TARGET_IDENTITY", marker)
            .env("BMUX_MANAGED_TARGET_ID", "a".repeat(64))
            .env("BMUX_MANAGED_IDLE_GRACE_SECONDS", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut owner = loop {
        assert!(child.0.try_wait().unwrap().is_none(), "{}", server_output());
        assert!(
            Instant::now() < deadline,
            "managed handshake stalled: {}",
            server_output()
        );
        match bmux_client::BmuxClient::connect(
            &endpoint,
            Duration::from_secs(1),
            "managed-readiness-test",
        )
        .await
        {
            Ok(client) => break client,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    };
    owner.require_service_contracts().unwrap();
    let ready = client::runtime_readiness(&mut owner).await.unwrap();
    assert_eq!(ready.managed_target_identity.as_deref(), Some(marker));
    // An ordinary cargo test binary may have no packaged supervisor. The
    // watcher must reach policy verification, then fail closed (not time out
    // on a self-Hello or reject the host origin).
    if !ready.helper_ready {
        loop {
            let output = server_output();
            if output.contains("managed idle policy requires ready, unfenced execution service") {
                assert!(!output.contains("ipc.request.timeout"));
                client::runtime_readiness(&mut owner).await.unwrap();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "watcher failed before policy verification: {output}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    loop {
        let status = client::managed_idle_status(&mut owner)
            .await
            .unwrap()
            .unwrap();
        if status.managed {
            assert_eq!(status.grace_seconds, 0);
            assert!(!status.fenced);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle watcher did not configure: {}",
            server_output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Both the host watcher and transport caller remain live after configuration.
    client::runtime_readiness(&mut owner).await.unwrap();
    assert!(
        !server_output().contains("idle watcher disabled"),
        "{}",
        server_output()
    );
}

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
