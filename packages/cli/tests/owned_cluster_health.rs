#![cfg(all(
    feature = "bundled-plugin-cluster",
    feature = "bundled-plugin-connections"
))]
#[allow(dead_code)] // The shared process fixture also supports multi-server and SSH tests.
mod support;

use bmux_cluster_plugin_api::{cluster_query, cluster_types::ClusterHostState};
use std::time::Duration;
use support::ServerEnv;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cluster_checks_use_owner_in_selected_server_but_cannot_repair() {
    let mut server = ServerEnv::new("cluster-health-scope");
    server.write_config("[connections.targets.fixture]\ntransport='local'\n[plugins.settings.\"bmux.cluster\".clusters.fleet]\ntargets=['fixture']\n");
    server.start();
    let mut client = bmux_client::BmuxClient::connect(
        &bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock")),
        Duration::from_secs(10),
        "cluster-health-integration",
    )
    .await
    .unwrap();
    for doctor in [false, true] {
        let status = cluster_query::client::status(&mut client, Some("fleet".into()), Some(doctor))
            .await
            .unwrap();
        assert_eq!(status.statuses.len(), 1);
        let host = &status.statuses[0];
        assert_eq!(host.target, "fixture");
        assert_eq!(host.state, ClusterHostState::Ready, "{:?}", host.reason);
    }
    let repair = bmux_plugin_sdk::cli_bridge::cli_command::client::run_plugin(
        &mut client,
        bmux_plugin_sdk::CORE_CLI_BRIDGE_PROTOCOL_V1,
        "bmux.connections".into(),
        "remote-doctor".into(),
        vec!["fixture".into(), "--fix".into()],
    )
    .await
    .expect("nested repair request must reach the command owner");
    assert_ne!(repair.exit_code, 0);
    assert!(
        repair
            .error
            .as_deref()
            .is_some_and(|message| message.contains("read-only health probe")),
        "unexpected denial: {repair:?}"
    );
}
