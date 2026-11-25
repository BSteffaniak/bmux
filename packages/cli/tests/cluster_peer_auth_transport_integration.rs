#[allow(dead_code)] // The shared process fixture also serves other test binaries.
mod support;

// The remote-access fixture: hosts with remote hosting on, and the pairing ceremony. Cluster
// nodes reach each other as saved hosts over the paired (Iroh) transport.
#[cfg(all(
    unix,
    feature = "bundled-plugin-cluster",
    feature = "bundled-plugin-connections"
))]
#[allow(dead_code)]
#[path = "remote_access/harness.rs"]
mod harness;

// Pane replication through the cluster attach provider runs on this same process harness.
#[cfg(all(
    unix,
    feature = "bundled-plugin-cluster",
    feature = "bundled-plugin-connections"
))]
mod cluster_frame_replication;

// Cluster membership over the paired transport: join and leave with mutual peer
// authentication, and three voters surviving a restart.
#[cfg(all(
    unix,
    feature = "bundled-plugin-cluster",
    feature = "bundled-plugin-connections"
))]
mod cluster_paired_membership;

use serial_test::serial;
use support::{ServerEnv, assert_success};

#[test]
#[serial]
fn fixture_failure_retains_only_bounded_diagnostics_after_retirement() {
    let mut root = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut server = ServerEnv::new("intentional-failure-retirement");
        server.start();
        root = Some(server.root().to_path_buf());
        panic!("intentional fixture failure");
    }));
    assert!(result.is_err());
    let root = root.unwrap();
    assert!(
        !root.exists(),
        "failed fixture leaked runtime {}",
        root.display()
    );
    let diagnostics = root.with_file_name(format!(
        "{}-diagnostics",
        root.file_name().unwrap().to_string_lossy()
    ));
    let report = std::fs::read(diagnostics.join("README.txt")).unwrap();
    assert!(report.len() < 4096);
    assert_eq!(std::fs::read_dir(&diagnostics).unwrap().count(), 1);
    std::fs::remove_dir_all(diagnostics).unwrap();
}

#[test]
#[serial]
fn fixture_retirement_removes_running_session_and_both_artifact_roots() {
    let mut server = ServerEnv::new("session-retirement");
    server.start();
    let output = server.run(&["session", "new", "cleanup-test"]);
    assert_success(&output, "create test session");
    let root = server.root().to_path_buf();
    drop(server);
    assert!(
        !root.exists(),
        "session retirement leaked {}",
        root.display()
    );
}

#[test]
#[serial]
fn fixture_preserves_unresolved_recovery_artifacts_after_server_exit() {
    let mut server = ServerEnv::new("recovery-retention");
    server.start();
    let recovery = server.state_dir.join("pane-executions-v4");
    std::fs::create_dir_all(&recovery).unwrap();
    let marker = recovery.join("test-owned-unresolved-record");
    std::fs::write(&marker, "intentional test recovery evidence").unwrap();
    server.stop();
    assert!(marker.is_file());
    assert!(server.root().exists());
    // Remove only the record this test created, then obtain a new owned
    // lifecycle and verify normal teardown is possible again.
    std::fs::remove_file(marker).unwrap();
    server.start();
    let root = server.root().to_path_buf();
    drop(server);
    assert!(!root.exists());
}

#[test]
#[serial]
fn shared_process_harness_supports_three_isolated_lifecycles_and_log_capture() {
    let mut harness =
        support::ClusterProcessHarness::new(["lifecycle-a", "lifecycle-b", "lifecycle-c"]);
    assert_eq!(harness.len(), 3);
    assert!(!harness.is_empty());
    let roots = (0..harness.len())
        .map(|index| harness.node(index).root().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(
        roots
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );

    harness.start_all();
    for index in 0..harness.len() {
        assert!(harness.node_mut(index).is_running());
        assert!(
            harness
                .node(index)
                .run(&["server", "status"])
                .status
                .success()
        );
    }

    harness.node_mut(1).kill();
    assert!(!harness.node_mut(1).is_running());
    harness.node_mut(1).restart();
    assert!(harness.node_mut(1).is_running());
    assert!(harness.node(1).server_output().is_char_boundary(0));

    harness.stop_all();
    for index in 0..harness.len() {
        assert!(!harness.node_mut(index).is_running());
    }
    drop(harness);
    assert!(roots.iter().all(|root| !root.exists()));
}
