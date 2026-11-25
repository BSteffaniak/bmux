//! Cluster health checks run in the selected server, against the hosts that server's
//! machine has paired with, over the paired transport only. A check is read-only: it needs
//! no more than the `observe` class, and nothing it reaches can repair or change the member.
#![cfg(all(
    unix,
    feature = "bundled-plugin-cluster",
    feature = "bundled-plugin-connections"
))]
#[allow(dead_code)] // The shared process fixture also supports multi-server tests.
mod support;

#[allow(dead_code)] // The remote-access fixture; this target uses hosts and pairing.
#[path = "remote_access/harness.rs"]
mod harness;

use bmux_cluster_plugin_api::{cluster_query, cluster_types::ClusterHostState};
use harness::{Host, RemoteSettings, Role, pair_only, reserve_udp_port};
use std::time::Duration;

const MEMBER: &str = "fixture";

#[test]
fn cluster_checks_use_owner_in_selected_server_but_cannot_repair() {
    let member = Host::start(
        "health-member",
        RemoteSettings::loopback_host(reserve_udp_port()),
    );
    member.new_session("work");
    // The owner's cluster names a saved host, a name it never paired with, and the
    // local runtime, which is never a member endpoint.
    let owner = Host::start_with(
        "health-owner",
        RemoteSettings::loopback_host(reserve_udp_port()),
        &format!(
            "[plugins.settings.\"bmux.cluster\".clusters.fleet]\ntargets = ['{MEMBER}', 'unpaired', 'local']\n"
        ),
    );
    // View-only: a health check is an `observe` read.
    pair_only(&member, &owner, MEMBER, "health-owner", Role::ViewOnly);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        // The owner's selected server resolves the member in its own saved hosts; this
        // test process's profile has none.
        let mut client = bmux_client::BmuxClient::connect(
            &bmux_ipc::IpcEndpoint::UnixSocket(owner.env.runtime_dir.join("server.sock")),
            Duration::from_secs(30),
            "cluster-health-integration",
        )
        .await
        .expect("connect to the owner's server");
        for doctor in [false, true] {
            let status =
                cluster_query::client::status(&mut client, Some("fleet".into()), Some(doctor))
                    .await
                    .expect("cluster status");
            assert_eq!(status.statuses.len(), 3, "{status:?}");
            for host in &status.statuses {
                if host.target == MEMBER {
                    assert_eq!(host.state, ClusterHostState::Ready, "{:?}", host.reason);
                } else {
                    assert_eq!(host.state, ClusterHostState::Degraded, "{host:?}");
                    assert!(
                        host.reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("is not a saved host")),
                        "{host:?}"
                    );
                }
            }
        }
    });

    // The CLI in the owner's environment reports the same.
    let doctor = owner.run(&["cluster", "doctor", "fleet"]);
    let text = support::combined_output(&doctor);
    assert!(text.contains(MEMBER), "{text}");

    // Repair is refused: the same saved host refuses any change from the device the
    // checks use, and nothing on the member changed.
    let repair = owner.run(&["--target", MEMBER, "new-session", "repair"]);
    assert!(
        !repair.status.success(),
        "a read-only member check's device changed the member: {}",
        support::combined_output(&repair)
    );
    let sessions = member.ok(&["list-sessions"], "list member sessions");
    assert!(sessions.contains("work"), "{sessions}");
    assert!(!sessions.contains("repair"), "{sessions}");
}
