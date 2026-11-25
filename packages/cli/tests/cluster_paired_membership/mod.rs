//! Cluster membership over the paired transport: every node pairs with the others (each saved
//! under the other's node name) and reaches them as saved hosts through `bmux.connections`.
use crate::harness::{Host, RemoteSettings, Role, pair_only, reserve_udp_port};
use crate::support::{ServerEnv, combined_output};
use serial_test::serial;
use std::time::Duration;

fn host(label: &str) -> Host {
    Host::start(label, RemoteSettings::loopback_host(reserve_udp_port()))
}

/// Pairs every node's own profile with every node, itself included, each saved under that
/// node's name: a member reaches every member, itself too, by its advertised endpoint.
/// Pairing never grants cluster membership; enrollment does.
pub fn pair_mesh(nodes: &[(&Host, &str)]) {
    for (host, host_name) in nodes {
        for (connecting, connecting_name) in nodes {
            let device = if host_name == connecting_name {
                format!("{connecting_name}-self")
            } else {
                (*connecting_name).to_owned()
            };
            pair_only(host, *connecting, host_name, &device, Role::Full);
        }
    }
}

fn assert_success_with_server_logs(
    output: &std::process::Output,
    what: &str,
    nodes: &[&ServerEnv],
) {
    assert!(
        output.status.success(),
        "{what} failed with {:?}: {}\nserver logs:\n{}",
        output.status.code(),
        combined_output(output),
        nodes
            .iter()
            .map(|node| format!("root={}\n{}", node.root().display(), node.server_output()))
            .collect::<Vec<_>>()
            .join("\n---\n")
    );
}

/// `cluster members` stdout only: stderr carries timestamped logs.
fn members(node: &Host) -> String {
    let output = node.run(&["cluster", "members"]);
    crate::support::assert_success(&output, "list members");
    String::from_utf8(output.stdout).expect("members output is UTF-8")
}

fn enrollment_token(issuer: &Host, endpoint: &str, voter: bool) -> String {
    let mut args = vec![
        "cluster",
        "enrollment-token",
        "create",
        "--endpoint",
        endpoint,
    ];
    if voter {
        args.extend(["--role", "voter", "--worker", "--ingress"]);
    }
    issuer
        .ok(&args, "create enrollment token")
        .lines()
        .find(|line| line.starts_with("bmux-enroll-v1:"))
        .expect("enrollment output contains a token")
        .to_string()
}

/// Whether a failed cluster command met a leadership change it may outlast: an election
/// in progress (`NotLeader`) or an unavailable forward to a departed leader.
fn transient(text: &str) -> bool {
    text.contains("NotLeader {") || text.contains("RuntimeUnavailable {")
}

fn join(issuer: &Host, joiner: &Host, token: &str, issuer_name: &str, endpoint: &str) {
    for attempt in ["join", "retry the join after an assumed lost response"] {
        // A join is retry-safe with the same token; it waits out an election.
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        let output = loop {
            let output = joiner.run(&[
                "cluster",
                "join",
                token,
                "--issuer",
                issuer_name,
                "--endpoint",
                endpoint,
            ]);
            if output.status.success()
                || !transient(&combined_output(&output))
                || std::time::Instant::now() >= deadline
            {
                break output;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        assert_success_with_server_logs(&output, attempt, &[&issuer.env, &joiner.env]);
        assert!(combined_output(&output).contains("joined cluster:"));
    }
}

/// Reads membership, waiting out a leadership change after a voter stops or restarts: reads
/// then fail as `NotLeader` or as an unavailable forward to the departed leader until an
/// election completes.
fn read_members_through_leader_change(node: &ServerEnv) -> std::process::Output {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let output = node.run(&["cluster", "members"]);
        if output.status.success()
            || !transient(&combined_output(&output))
            || std::time::Instant::now() >= deadline
        {
            return output;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Asks `node`'s own server to run cluster administration on `target` over the paired
/// transport. A paired device is never a cluster authority: `init` is owner-local.
fn assert_pairing_is_not_cluster_authority(node: &Host, target: &str) {
    use bmux_cluster_plugin_api::cluster_command;
    use bmux_connections_plugin_api::{connection_types, connections_commands};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let mut connection = bmux_client::BmuxClient::connect(
            &bmux_ipc::IpcEndpoint::UnixSocket(node.env.runtime_dir.join("server.sock")),
            Duration::from_secs(30),
            "cluster-paired-authority",
        )
        .await
        .expect("connect to the node's own server");
        let payload = bmux_plugin_sdk::service_contract::encode(
            cluster_command::INTERFACE_ID.as_str(),
            cluster_command::SERVICE_CONTRACT,
            &bmux_plugin_sdk::encode_service_message(&cluster_command::client::InitRequest {
                endpoint: None,
            })
            .expect("encode init"),
        )
        .expect("encode service contract");
        let result = connections_commands::client::invoke_service(
            &mut connection,
            connection_types::ServiceInvocation {
                target: target.to_owned(),
                capability: "bmux.server_clusters.write".into(),
                kind: connection_types::ServiceInvokeKind::InvokeCommand,
                interface_id: cluster_command::INTERFACE_ID.as_str().into(),
                operation: "init".into(),
                payload,
                options: None,
            },
        )
        .await
        .expect("local owner invocation envelope");
        assert!(
            result.is_err(),
            "a paired device ran owner-local cluster administration: {result:?}"
        );
    });
}

#[test]
#[serial]
fn paired_join_and_leave_require_real_mutual_peer_authentication() {
    let issuer = host("paired-issuer");
    let joiner = host("paired-joiner");
    pair_mesh(&[(&issuer, "issuer"), (&joiner, "joiner")]);
    issuer.ok(
        &["cluster", "init", "--endpoint", "issuer"],
        "initialize issuer cluster",
    );
    assert_pairing_is_not_cluster_authority(&joiner, "issuer");
    let token = enrollment_token(&issuer, "issuer", false);
    join(&issuer, &joiner, &token, "issuer", "joiner");

    let issuer_members = members(&issuer);
    let joiner_members = members(&joiner);
    assert_eq!(
        issuer_members, joiner_members,
        "both nodes adopt the same public membership snapshot"
    );

    let status = joiner.ok(&["cluster", "status"], "status without liveness mutation");
    assert!(status.contains("cluster status identity"), "{status}");
    assert!(status.contains("liveness=Unchecked"), "{status}");
    let doctor = joiner.ok(
        &["cluster", "doctor"],
        "probe membership trust and reachability",
    );
    assert!(doctor.contains("cluster doctor identity"), "{doctor}");
    assert!(doctor.contains("liveness=Reachable"), "{doctor}");
    assert!(doctor.contains("compatible=true"), "{doctor}");
    assert!(doctor.contains("trusted=true"), "{doctor}");
    assert_eq!(
        members(&joiner),
        joiner_members,
        "health checks never change membership"
    );

    let leave = joiner.ok(
        &["cluster", "leave"],
        "leave with mutual peer authentication",
    );
    assert!(leave.contains("left cluster:"), "{leave}");
}

#[test]
#[serial]
fn three_paired_voters_form_membership_and_survive_one_node_restart() {
    let names = ["voter-0", "voter-1", "voter-2"];
    let mut nodes = names.map(host);
    pair_mesh(&[
        (&nodes[0], names[0]),
        (&nodes[1], names[1]),
        (&nodes[2], names[2]),
    ]);
    nodes[0].ok(
        &["cluster", "init", "--endpoint", names[0]],
        "initialize three-voter cluster",
    );
    for index in 1..3 {
        let token = enrollment_token(&nodes[0], names[0], true);
        join(&nodes[0], &nodes[index], &token, names[0], names[index]);
    }
    let voters = |output: &str| output.matches(" state=Active role=Voter ").count();
    for node in &nodes {
        let output = read_members_through_leader_change(&node.env);
        crate::support::assert_success(&output, "read three-voter membership");
        let members = combined_output(&output);
        assert_eq!(voters(&members), 3, "{members}");
    }

    nodes[2].env.kill();
    for node in &nodes[..2] {
        let members = read_members_through_leader_change(&node.env);
        assert_success_with_server_logs(
            &members,
            "read membership with one voter down",
            &[&nodes[0].env, &nodes[1].env],
        );
        assert_eq!(voters(&combined_output(&members)), 3);
    }
    // The restarted node resumes hosting (the persisted `remote enable`) and rejoins Raft
    // over the paired transport without manual intervention.
    nodes[2].env.restart();
    let recovered = read_members_through_leader_change(&nodes[2].env);
    assert_success_with_server_logs(
        &recovered,
        "read membership after voter restart",
        &[&nodes[0].env, &nodes[1].env, &nodes[2].env],
    );
    assert_eq!(voters(&combined_output(&recovered)), 3);
    let status = nodes[2].ok(&["remote", "status"], "hosting after restart");
    assert!(status.contains("enabled"), "{status}");
}
