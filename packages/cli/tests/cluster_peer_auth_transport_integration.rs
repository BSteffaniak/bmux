mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use support::{
    ProcessGuard, ServerEnv, assert_success, bmux_binary, combined_output, generate_ed25519_key,
    reserve_tcp_port,
};

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
fn ssh_join_and_leave_require_real_mutual_peer_authentication() {
    if !Path::new("/usr/sbin/sshd").exists() {
        eprintln!("skipping SSH transport integration: /usr/sbin/sshd unavailable");
        return;
    }
    let port = reserve_tcp_port();
    let mut issuer = ServerEnv::new("ssh-issuer");
    issuer.write_config("");
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
    joiner.write_config(&format!(
        "[connections.targets.issuer]\ntransport = \"ssh\"\nhost = \"127.0.0.1\"\nuser = \"{}\"\nport = {port}\nidentity_file = \"{}\"\nknown_hosts_file = \"{}\"\nstrict_host_key_checking = false\nremote_bmux_path = \"{}\"\nserver_start_mode = \"require_running\"\n",
        std::env::var("USER").expect("USER is set"),
        client_key.display(),
        known_hosts.display(),
        remote_bmux.display(),
    ));
    issuer.start();
    joiner.start();

    let token = create_enrollment_token(&issuer, "issuer");
    assert_join_and_leave(&issuer, &joiner, &token, "issuer");
}

#[test]
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

fn tls_cluster_config(own_port: u16, ports: &[u16; 3]) -> String {
    format!(
        "[general]\nserver_timeout = 30000\n\n[server.gateway]\nenabled = true\nlisten = \"127.0.0.1:{own_port}\"\nquick = true\n\n[connections.tls_trust]\nmode = \"trust_new\"\n\n[connections.targets.node-a]\ntransport = \"tls\"\nhost = \"127.0.0.1\"\nport = {}\nserver_name = \"localhost\"\nconnect_timeout_ms = 30000\nserver_start_mode = \"require_running\"\n\n[connections.targets.node-b]\ntransport = \"tls\"\nhost = \"127.0.0.1\"\nport = {}\nserver_name = \"localhost\"\nconnect_timeout_ms = 30000\nserver_start_mode = \"require_running\"\n\n[connections.targets.node-c]\ntransport = \"tls\"\nhost = \"127.0.0.1\"\nport = {}\nserver_name = \"localhost\"\nconnect_timeout_ms = 30000\nserver_start_mode = \"require_running\"\n",
        ports[0], ports[1], ports[2]
    )
}

#[test]
fn three_real_tls_voters_form_membership_and_survive_one_node_restart() {
    let ports = [reserve_tcp_port(), reserve_tcp_port(), reserve_tcp_port()];
    let mut harness =
        support::ClusterProcessHarness::new(["three-voter-a", "three-voter-b", "three-voter-c"]);
    for (index, port) in ports.iter().copied().enumerate() {
        harness
            .node(index)
            .write_config(&tls_cluster_config(port, &ports));
    }
    harness.start_all();

    let init = harness
        .node(0)
        .run(&["cluster", "init", "--endpoint", "node-a"]);
    assert_success(&init, "initialize real three-voter cluster");
    let token_b = create_voter_enrollment_token(harness.node(0), "node-a");
    join_voter(
        harness.node(0),
        harness.node(1),
        &token_b,
        "node-a",
        "node-b",
    );
    let token_c = create_voter_enrollment_token(harness.node(0), "node-a");
    join_voter(
        harness.node(0),
        harness.node(2),
        &token_c,
        "node-a",
        "node-c",
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
    let recovered = harness.node(2).run(&["cluster", "members"]);
    assert_success(&recovered, "read membership after voter restart");
    assert_eq!(
        combined_output(&recovered)
            .matches(" state=Active role=Voter ")
            .count(),
        3
    );
}

#[test]
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
fn tls_join_and_leave_require_real_mutual_peer_authentication() {
    let port = reserve_tcp_port();
    let mut issuer = ServerEnv::new("tls-issuer");
    issuer.write_config(&format!(
        "[server.gateway]\nenabled = true\nlisten = \"127.0.0.1:{port}\"\nquick = true\n"
    ));
    let mut joiner = ServerEnv::new("tls-joiner");
    joiner.write_config(&format!(
        "[connections.tls_trust]\nmode = \"trust_new\"\n\n[connections.targets.issuer]\ntransport = \"tls\"\nhost = \"127.0.0.1\"\nport = {port}\nserver_name = \"localhost\"\nserver_start_mode = \"require_running\"\n"
    ));

    issuer.start();
    joiner.start();

    let token = create_enrollment_token(&issuer, "issuer");
    assert_join_and_leave(&issuer, &joiner, &token, "issuer");
}
