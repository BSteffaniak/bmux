mod support;

use bmux_connections_plugin_api::{pairing_commands, pairing_state, pairing_types};
use serial_test::serial;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use support::{
    ProcessGuard, ServerEnv, assert_success, bmux_binary, combined_output, generate_ed25519_key,
    reserve_tcp_port,
};

async fn local_connection(server: &ServerEnv) -> bmux_client::BmuxClient {
    bmux_client::BmuxClient::connect(
        &bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock")),
        Duration::from_secs(30),
        "cluster-admission-integration",
    )
    .await
    .expect("connect to isolated local owner endpoint")
}

async fn start_admitted_listener(server: &ServerEnv) -> String {
    let mut connection = local_connection(server).await;
    let status = pairing_commands::client::start_listener(&mut connection)
        .await
        .expect("start plugin-owned admitted listener");
    assert!(status.enabled);
    status
        .endpoint_targets
        .into_iter()
        .find(|target| target.contains("127.0.0.1"))
        .expect("listener advertises a pinned loopback target")
}

async fn assert_idempotent_listener_start(
    connection: &mut bmux_client::BmuxClient,
    expected_targets: &[String],
) {
    for _ in 0..2 {
        let status = tokio::time::timeout(
            Duration::from_secs(5),
            pairing_commands::client::start_listener(connection),
        )
        .await
        .expect("listener start completes within five seconds with active Raft")
        .expect("owner idempotent listener start");
        assert!(status.enabled);
        assert_eq!(status.endpoint_targets, expected_targets);
    }
}

async fn pair_runtime(issuer: &ServerEnv, joining: &ServerEnv, endpoint: &str) {
    let mut joining_connection = local_connection(joining).await;
    let key = pairing_state::client::local_device_identity(&mut joining_connection)
        .await
        .expect("read joining runtime public device key");
    let mut issuer_connection = local_connection(issuer).await;
    let invitation = pairing_commands::client::create_invitation(
        &mut issuer_connection,
        key,
        60,
        pairing_types::DeviceAccess::FullLocalRuntime,
    )
    .await
    .expect("owner issues device-specific transport invitation");
    let paired = pairing_commands::client::pair_endpoint(
        &mut joining_connection,
        endpoint.to_owned(),
        invitation,
    )
    .await
    .expect("redeem invitation and retain outbound pairing");
    assert!(paired.starts_with("device://"));
}

fn create_enrollment_token(issuer: &ServerEnv, endpoint: &str) -> String {
    let init = issuer.run(&["cluster", "init", "--endpoint", endpoint]);
    assert_success(&init, "initialize issuer cluster");
    let enrollment = issuer.run(&[
        "cluster",
        "enrollment-token",
        "create",
        "--endpoint",
        endpoint,
    ]);
    assert_success(&enrollment, "create enrollment token");
    String::from_utf8(enrollment.stdout)
        .expect("token output is UTF-8")
        .lines()
        .find(|line| line.starts_with("bmux-enroll-v1:"))
        .expect("enrollment output contains token")
        .to_string()
}

fn assert_join_and_leave(issuer: &ServerEnv, joiner: &ServerEnv, token: &str, target: &str) {
    let join = joiner.run(&[
        "cluster",
        "join",
        token,
        "--issuer",
        target,
        "--endpoint",
        "joiner",
    ]);
    assert_success(&join, "join with mutual peer authentication");
    assert!(
        combined_output(&join).contains("joined cluster:"),
        "unexpected join output: {}",
        combined_output(&join)
    );

    let issuer_members = issuer.run(&["cluster", "members"]);
    assert_success(&issuer_members, "list issuer members");
    let joiner_members = joiner.run(&["cluster", "members"]);
    assert_success(&joiner_members, "list joiner members");
    assert_eq!(
        String::from_utf8_lossy(&issuer_members.stdout),
        String::from_utf8_lossy(&joiner_members.stdout),
        "both nodes should adopt the same public membership snapshot"
    );

    let status = joiner.run(&["cluster", "status"]);
    assert_success(
        &status,
        "report membership status without liveness mutation",
    );
    let status_output = combined_output(&status);
    assert!(status_output.contains("cluster status identity"));
    assert!(status_output.contains("liveness=Unchecked"));
    let members_after_status = joiner.run(&["cluster", "members"]);
    assert_success(&members_after_status, "list members after status");
    assert_eq!(joiner_members.stdout, members_after_status.stdout);

    let doctor = joiner.run(&["cluster", "doctor"]);
    assert_success(&doctor, "probe membership trust and reachability");
    let doctor_output = combined_output(&doctor);
    assert!(doctor_output.contains("cluster doctor identity"));
    assert!(doctor_output.contains("liveness=Reachable"));
    assert!(doctor_output.contains("compatible=true"));
    assert!(doctor_output.contains("trusted=true"));
    let members_after_doctor = joiner.run(&["cluster", "members"]);
    assert_success(&members_after_doctor, "list members after doctor");
    assert_eq!(joiner_members.stdout, members_after_doctor.stdout);

    let leave = joiner.run(&["cluster", "leave"]);
    assert_success(&leave, "leave with mutual peer authentication");
    assert!(
        combined_output(&leave).contains("left cluster:"),
        "unexpected leave output: {}",
        combined_output(&leave)
    );
}

#[test]
#[serial]
fn legacy_ssh_bridge_cannot_redeem_cluster_enrollment() {
    if !Path::new("/usr/sbin/sshd").exists() {
        eprintln!("skipping SSH transport integration: /usr/sbin/sshd unavailable");
        return;
    }
    let port = reserve_tcp_port();
    let mut issuer = ServerEnv::new("ssh-issuer");
    // Match the TLS fixture's integration budget: SSH startup and enrollment
    // redemption must fit inside the outer IPC request under full-suite load.
    issuer.write_config("[general]\nserver_timeout = 30000\n");
    let mut joiner = ServerEnv::new("ssh-joiner");
    let ssh_root = joiner.root().join("sshd");
    std::fs::create_dir_all(&ssh_root).expect("create SSH test root");
    let host_key = ssh_root.join("host-key");
    let client_key = ssh_root.join("client-key");
    let authorized_keys = ssh_root.join("authorized_keys");
    let known_hosts = ssh_root.join("known_hosts");
    let remote_bmux = ssh_root.join("bmux-issuer");
    generate_ed25519_key(&host_key);
    generate_ed25519_key(&client_key);
    std::fs::copy(client_key.with_extension("pub"), &authorized_keys)
        .expect("write SSH authorized_keys");
    let mut permissions = std::fs::metadata(&authorized_keys)
        .expect("read authorized_keys metadata")
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
        std::fs::set_permissions(&authorized_keys, permissions)
            .expect("set authorized_keys permissions");
    }
    let sshd_config = ssh_root.join("sshd_config");
    std::fs::write(
        &sshd_config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nChallengeResponseAuthentication no\nUsePAM no\nPermitRootLogin no\nStrictModes no\nLogLevel ERROR\n",
            host_key.display(),
            ssh_root.join("sshd.pid").display(),
            authorized_keys.display(),
        ),
    )
    .expect("write sshd config");
    let sshd = Command::new("/usr/sbin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(&sshd_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn isolated sshd");
    let _sshd = ProcessGuard::new(sshd);
    thread::sleep(Duration::from_millis(250));

    std::fs::write(
        &remote_bmux,
        format!(
            "#!/bin/sh\nBMUX_RUNTIME_DIR='{}' BMUX_CONFIG_DIR='{}' BMUX_DATA_DIR='{}' BMUX_STATE_DIR='{}' BMUX_LOG_DIR='{}' exec '{}' \"$@\"\n",
            issuer.runtime_dir.display(),
            issuer.config_dir.display(),
            issuer.data_dir.display(),
            issuer.state_dir.display(),
            issuer.log_dir.display(),
            bmux_binary().display(),
        ),
    )
    .expect("write remote bmux wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut wrapper_permissions = std::fs::metadata(&remote_bmux)
            .expect("read remote wrapper metadata")
            .permissions();
        wrapper_permissions.set_mode(0o700);
        std::fs::set_permissions(&remote_bmux, wrapper_permissions)
            .expect("set remote wrapper permissions");
    }
    // Readiness launches an SSH process and a remote CLI. Give that inner
    // deadline its own full-suite budget, below the 30-second IPC deadline.
    joiner.write_config(&format!(
        "[general]\nserver_timeout = 30000\n\n[connections.targets.issuer]\ntransport = \"ssh\"\nconnect_timeout_ms = 20000\nhost = \"127.0.0.1\"\nuser = \"{}\"\nport = {port}\nidentity_file = \"{}\"\nknown_hosts_file = \"{}\"\nstrict_host_key_checking = false\nremote_bmux_path = \"{}\"\nserver_start_mode = \"require_running\"\n",
        std::env::var("USER").expect("USER is set"),
        client_key.display(),
        known_hosts.display(),
        remote_bmux.display(),
    ));
    issuer.start();
    joiner.start();

    let token = create_enrollment_token(&issuer, "issuer");
    // SSH authentication is not trusted stream admission. Keep this legacy
    // path fail-closed; the admitted transport below covers successful joining.
    let join = joiner.run(&[
        "cluster",
        "join",
        &token,
        "--issuer",
        "issuer",
        "--endpoint",
        "issuer",
    ]);
    assert!(
        !join.status.success(),
        "legacy SSH bridge unexpectedly redeemed enrollment"
    );
    assert!(
        combined_output(&join).contains("remote enrollment redemption failed"),
        "{}",
        combined_output(&join)
    );
    let log = std::fs::read_to_string(joiner.root().join("server.stderr.log"))
        .expect("joiner server diagnostics");
    assert!(
        log.contains("legacy remote ingress cannot establish trusted stream admission"),
        "{log}"
    );
}

#[test]
#[serial]
#[ignore = "blocked on macOS iroh/netwatch CoreWLAN endpoint-bind stall"]
fn iroh_join_and_leave_require_real_mutual_peer_authentication() {
    let mut issuer = ServerEnv::new("iroh-issuer");
    let issuer_url = issuer.start_iroh_gateway();
    let mut joiner = ServerEnv::new("iroh-joiner");
    joiner.write_config(&format!(
        "[connections.targets.issuer]\ntransport = \"iroh\"\nendpoint_id = \"{}\"\niroh_ip_addr = \"{}\"\nserver_start_mode = \"require_running\"\n",
        issuer_url
            .strip_prefix("iroh://")
            .expect("iroh URL scheme")
            .split(['?', '/'])
            .next()
            .expect("iroh endpoint ID"),
        issuer_url
            .split_once("addr=")
            .map(|(_, addr)| addr.split('&').next().unwrap_or(addr))
            .expect("direct iroh URL contains socket address")
    ));
    joiner.start();

    let token = create_enrollment_token(&issuer, "issuer");
    assert_join_and_leave(&issuer, &joiner, &token, "issuer");
}

fn create_voter_enrollment_token(issuer: &ServerEnv, issuer_endpoint: &str) -> String {
    let enrollment = issuer.run(&[
        "cluster",
        "enrollment-token",
        "create",
        "--endpoint",
        issuer_endpoint,
        "--role",
        "voter",
        "--worker",
        "--ingress",
    ]);
    assert_success(&enrollment, "create voter enrollment token");
    String::from_utf8(enrollment.stdout)
        .expect("token output is UTF-8")
        .lines()
        .find(|line| line.starts_with("bmux-enroll-v1:"))
        .expect("enrollment output contains token")
        .to_string()
}

fn assert_success_with_server_logs(
    output: &std::process::Output,
    operation: &str,
    nodes: &[&ServerEnv],
) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}: {}\nserver logs:\n{}",
        output.status.code(),
        combined_output(output),
        nodes
            .iter()
            .map(|node| format!("root={}\n{}", node.root().display(), node.server_output()))
            .collect::<Vec<_>>()
            .join("\n---\n")
    );
}

fn join_voter(
    issuer_node: &ServerEnv,
    joiner: &ServerEnv,
    token: &str,
    issuer: &str,
    endpoint: &str,
) {
    let join = joiner.run(&[
        "cluster",
        "join",
        token,
        "--issuer",
        issuer,
        "--endpoint",
        endpoint,
    ]);
    assert_success_with_server_logs(
        &join,
        "join voter through authenticated endpoint",
        &[issuer_node, joiner],
    );
    assert!(combined_output(&join).contains("joined cluster:"));

    let retry = joiner.run(&[
        "cluster",
        "join",
        token,
        "--issuer",
        issuer,
        "--endpoint",
        endpoint,
    ]);
    assert_success_with_server_logs(
        &retry,
        "retry voter join after an assumed lost success response",
        &[issuer_node, joiner],
    );
    assert!(combined_output(&retry).contains("joined cluster:"));
}

fn admitted_cluster_config(port: u16) -> String {
    format!(
        "[general]\nserver_timeout = 30000\n\n[plugins.settings.\"bmux.connections\".remote]\nallow_full_runtime_ingress = true\nauto_start = true\nbind_addr = \"127.0.0.1:{port}\"\n\n[plugins.settings.\"bmux.connections\".remote.relay]\nmode = \"disabled\"\n"
    )
}

#[test]
#[serial]
fn three_admitted_voters_form_membership_and_survive_one_node_restart() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let mut harness =
        support::ClusterProcessHarness::new(["three-voter-a", "three-voter-b", "three-voter-c"]);
    for index in 0..harness.len() {
        harness
            .node(index)
            .write_config(&admitted_cluster_config(reserve_tcp_port()));
    }
    harness.start_all();
    let endpoints: Vec<_> = (0..harness.len())
        .map(|index| runtime.block_on(start_admitted_listener(harness.node(index))))
        .collect();
    for (issuer, endpoint) in endpoints.iter().enumerate() {
        for joining in 0..harness.len() {
            if issuer != joining {
                runtime.block_on(pair_runtime(
                    harness.node(issuer),
                    harness.node(joining),
                    endpoint,
                ));
            }
        }
    }

    let init = harness
        .node(0)
        .run(&["cluster", "init", "--endpoint", &endpoints[0]]);
    assert_success(&init, "initialize real three-voter cluster");
    let token_b = create_voter_enrollment_token(harness.node(0), &endpoints[0]);
    join_voter(
        harness.node(0),
        harness.node(1),
        &token_b,
        &endpoints[0],
        &endpoints[1],
    );
    let token_c = create_voter_enrollment_token(harness.node(0), &endpoints[0]);
    join_voter(
        harness.node(0),
        harness.node(2),
        &token_c,
        &endpoints[0],
        &endpoints[2],
    );

    for index in 0..harness.len() {
        let members = harness.node(index).run(&["cluster", "members"]);
        assert_success(&members, "read replicated three-voter membership");
        let output = combined_output(&members);
        assert_eq!(
            output.matches(" state=Active role=Voter ").count(),
            3,
            "unexpected membership on node {index}: {output}"
        );
    }

    let mut owner_connection = runtime.block_on(local_connection(harness.node(2)));
    let owner_status = runtime
        .block_on(pairing_state::client::listener_status(
            &mut owner_connection,
        ))
        .expect("read listener status with active three-voter Raft");
    assert!(owner_status.enabled);
    assert!(owner_status.endpoint_targets.contains(&endpoints[2]));
    runtime.block_on(assert_idempotent_listener_start(
        &mut owner_connection,
        &owner_status.endpoint_targets,
    ));
    drop(owner_connection);

    harness.node_mut(2).kill();
    for index in 0..2 {
        let members = harness.node(index).run(&["cluster", "members"]);
        assert_success(&members, "read membership with one voter down");
        assert_eq!(
            combined_output(&members)
                .matches(" state=Active role=Voter ")
                .count(),
            3
        );
    }
    harness.node_mut(2).restart();
    let mut restarted_connection = runtime.block_on(local_connection(harness.node(2)));
    let restarted_status = runtime
        .block_on(pairing_state::client::listener_status(
            &mut restarted_connection,
        ))
        .expect("restart restores admitted listener without manual intervention");
    assert!(restarted_status.enabled);
    assert_eq!(
        restarted_status.endpoint_targets,
        owner_status.endpoint_targets
    );
    runtime.block_on(assert_idempotent_listener_start(
        &mut restarted_connection,
        &owner_status.endpoint_targets,
    ));
    // IPC readiness precedes Raft leader discovery after restart. Wait for that
    // specific transient state, while failing immediately on other read errors.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let recovered = loop {
        let output = harness.node(2).run(&["cluster", "members"]);
        if output.status.success()
            || !combined_output(&output)
                .contains("NotLeader { leader_node_id: None, leader_endpoint: None }")
            || std::time::Instant::now() >= deadline
        {
            break output;
        }
        thread::sleep(Duration::from_millis(100));
    };
    assert_success_with_server_logs(
        &recovered,
        "read membership after voter restart",
        &[harness.node(0), harness.node(1), harness.node(2)],
    );
    assert_eq!(
        combined_output(&recovered)
            .matches(" state=Active role=Voter ")
            .count(),
        3
    );
    runtime.block_on(assert_idempotent_listener_start(
        &mut restarted_connection,
        &owner_status.endpoint_targets,
    ));
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

#[test]
#[serial]
fn admitted_join_and_leave_require_real_mutual_peer_authentication() {
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    let mut issuer = ServerEnv::new("admitted-issuer");
    let mut joiner = ServerEnv::new("admitted-joiner");
    issuer.write_config(&admitted_cluster_config(reserve_tcp_port()));
    joiner.write_config(&admitted_cluster_config(reserve_tcp_port()));
    issuer.start();
    joiner.start();
    let endpoint = runtime.block_on(start_admitted_listener(&issuer));
    runtime.block_on(pair_runtime(&issuer, &joiner, &endpoint));
    let token = create_enrollment_token(&issuer, &endpoint);
    runtime.block_on(assert_transport_is_not_cluster_authority(
        &joiner, &endpoint,
    ));
    assert_join_and_leave(&issuer, &joiner, &token, &endpoint);
}

#[test]
#[serial]
fn legacy_tls_ingress_cannot_bypass_stream_admission() {
    let server = ServerEnv::new("legacy-tls-admission-denial");
    let port = reserve_tcp_port();
    server.write_config(&format!(
        "[server.gateway]\nenabled = true\nlisten = \"127.0.0.1:{port}\"\nquick = true\n"
    ));
    let output = server.run(&["server", "start", "--foreground-internal"]);
    assert!(!output.status.success());
    assert!(
        combined_output(&output)
            .contains("legacy remote ingress cannot establish trusted stream admission"),
        "legacy ingress must fail closed: {}",
        combined_output(&output)
    );
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
}

async fn assert_transport_is_not_cluster_authority(joiner: &ServerEnv, endpoint: &str) {
    use bmux_connections_plugin_api::{connection_types, connections_commands};
    let mut connection = local_connection(joiner).await;
    let payload = bmux_plugin_sdk::service_contract::encode(
        bmux_cluster_plugin_api::cluster_command::INTERFACE_ID.as_str(),
        bmux_cluster_plugin_api::cluster_command::SERVICE_CONTRACT,
        &bmux_plugin_sdk::encode_service_message(
            &bmux_cluster_plugin_api::cluster_command::client::InitRequest { endpoint: None },
        )
        .unwrap(),
    )
    .unwrap();
    let result = connections_commands::client::invoke_service(
        &mut connection,
        connection_types::ServiceInvocation {
            target: endpoint.to_owned(),
            capability: "bmux.server_clusters.write".into(),
            kind: connection_types::ServiceInvokeKind::InvokeCommand,
            interface_id: "cluster-command/v1".into(),
            operation: "init".into(),
            payload,
            options: None,
        },
    )
    .await
    .expect("local owner invocation envelope");
    assert!(
        result.is_err(),
        "pairing must not expose cluster administration"
    );
}

// Reuse only the coordinator's bounded, secret-redacting PTY primitive. Rust
// retains ownership of the single host daemon and all persistence/revoke checks.
fn pair_daemonless_cli(host: &ServerEnv, laptop: &ServerEnv) {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/smoke-remote-pairing.py");
    let client_environment: std::collections::BTreeMap<_, _> = laptop
        .command(&[])
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
        })
        .collect();
    let output = Command::new("python3")
        .args(["-c", r#"
import importlib.util, json, os, signal, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location("pairing_smoke", sys.argv[2])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
signal.alarm(120)
client_env = dict(os.environ, **json.loads(sys.argv[3]))
owner = module.Terminal(Path(sys.argv[1]), os.environ, ["remote", "pair"])
connecting = None
try:
    link = owner.expect(rb"(bmux-pair:v1:[0-9a-f]+)").group(1)
    connecting = module.Terminal(Path(sys.argv[1]), client_env, ["connect", "--pair", "--name", "workstation", "--device-name", "laptop"])
    connecting.expect(rb"Pairing link.*: ")
    connecting.send(link + b"\n")
    phrase = rb"([0-9a-f]{4}(?:-[0-9a-f]{4}){3})"
    host_phrase = owner.expect(rb"Compare this phrase on the other device: " + phrase).group(1)
    client_phrase = connecting.expect(rb"Verify on the host: " + phrase).group(1)
    assert host_phrase == client_phrase, "authenticated verification phrases must match"
    owner.expect(rb"\[y/N\]")
    owner.send(b"y\n")
    connecting.expect(rb"Paired\. Saved host: workstation")
    assert owner.process.wait(timeout=10) == 0, "host approval failed"
    connecting.expect(rb"\x1b\[\?2004h")
finally:
    if connecting is not None:
        connecting.close()
    owner.close()
"#])
        .arg(bmux_binary()).arg(script).arg(serde_json::to_string(&client_environment).unwrap())
        .envs(host.command(&[]).get_envs().filter_map(|(key, value)| value.map(|value| (key, value))))
        .stdin(Stdio::null()).output().expect("run isolated interactive pairing harness");
    assert_success(
        &output,
        "host invitation and daemonless connect --pair with phrase approval",
    );
}

#[test]
#[serial_test::serial]
fn account_free_cli_pairs_persists_and_revokes() {
    let mut host = ServerEnv::new("remote-ux-host");
    let laptop = ServerEnv::new("remote-ux-laptop");
    let port = reserve_tcp_port();
    host.write_config(&format!("[plugins.settings.\"bmux.connections\".remote]\nbind_addr = '127.0.0.1:{port}'\n[plugins.settings.\"bmux.connections\".remote.relay]\nmode = 'disabled'\n"));
    laptop.write_config("");
    host.start();
    assert_success(
        &host.run(&["remote", "enable", "--yes"]),
        "enable host listener",
    );
    pair_daemonless_cli(&host, &laptop);
    assert!(
        !laptop.runtime_dir.join("server.sock").exists(),
        "pairing must not start a laptop daemon"
    );
    let status = laptop.run(&["remote", "status", "workstation"]);
    assert_success(&status, "named host admits paired device");
    assert!(combined_output(&status).contains("saved on this client"));
    let paths = bmux_config::ConfigPaths::new(
        laptop.config_dir.clone(),
        laptop.runtime_dir.clone(),
        laptop.data_dir.clone(),
        laptop.state_dir.clone(),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        use bmux_client::{
            AttachInputPayload, AttachProviderInput, AttachProviderViewport, AttachSession,
        };
        let mut terminal = bmux_connections_plugin::terminal_attach::RemoteTerminal::open(
            paths.clone(),
            "workstation".into(),
            "main".into(),
        )
        .await
        .expect("open authorized remote terminal");
        let initial = terminal.initial_snapshot().await.unwrap();
        let cursor = initial.streams[0].cursor.clone();
        terminal
            .send_input(AttachProviderInput {
                command_sequence: 1,
                stream_id: cursor.stream_id.clone(),
                generation: cursor.generation,
                payload: AttachInputPayload::Bytes(b"printf 'REMOTE_UX_PROOF\\n'\n".to_vec()),
            })
            .await
            .unwrap();
        terminal
            .update_viewport(AttachProviderViewport {
                command_sequence: 2,
                columns: 100,
                rows: 30,
                pixel_width: None,
                pixel_height: None,
            })
            .await
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let event = terminal.next_event().await.unwrap();
            if let bmux_client::AttachProviderEvent::Delta(delta) = event
                && delta.changes.iter().any(|change| matches!(change, bmux_client::AttachProviderChange::StreamRepair(stream) if String::from_utf8_lossy(&stream.snapshot).contains("REMOTE_UX_PROOF"))) { break; }
            assert!(
                std::time::Instant::now() < deadline,
                "remote PTY output not observed"
            );
        }
        terminal.detach().await.unwrap();
        let mut resumed = bmux_connections_plugin::terminal_attach::RemoteTerminal::open(
            paths.clone(),
            "workstation".into(),
            "main".into(),
        )
        .await
        .expect("resume same remote execution");
        assert_eq!(
            resumed.initial_snapshot().await.unwrap().scene.session_id,
            initial.scene.session_id
        );
    });
    host.restart();
    assert_success(
        &laptop.run(&["remote", "status", "workstation"]),
        "trust and listener survive restart",
    );
    runtime.block_on(async {
        bmux_connections_plugin::connect_paired(&paths, "workstation")
            .await
            .expect("durable trust authenticates after host restart without laptop server");
    });
    let revoke = host.run(&["remote", "revoke", "laptop"]);
    assert_success(&revoke, "owner revokes named device");
    runtime.block_on(async {
        assert!(
            bmux_connections_plugin::connect_paired(&paths, "workstation")
                .await
                .is_err(),
            "revoked device must not reauthenticate"
        );
    });
    assert_success(
        &host.run(&["remote", "disable"]),
        "disable admitted listener",
    );
    assert!(
        !laptop.runtime_dir.join("server.sock").exists(),
        "client must remain daemonless after restart and revoke"
    );
}
