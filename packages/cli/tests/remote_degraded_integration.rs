#[allow(dead_code)]
// This fixture uses local startup; SSH/process helpers serve other integration suites.
mod support;

use bmux_connections_plugin_api::{pairing_commands, pairing_state};
use support::{ServerEnv, assert_success};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn occupied_remote_port_preserves_local_terminal_and_explicit_retry() {
    let occupied = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap();
    let mut server = ServerEnv::new("remote-degraded");
    server.isolate_shell_config();
    server.write_config(&format!(
        "[general]\ndefault_shell='/bin/sh'\n[plugins.settings.\"bmux.connections\".remote]\nallow_full_runtime_ingress=true\nauto_start=true\nbind_addr='{address}'\n[plugins.settings.\"bmux.connections\".remote.relay]\nmode='disabled'\n"
    ));
    server.start();
    let mut client = bmux_client::BmuxClient::connect(
        &bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock")),
        std::time::Duration::from_secs(10),
        "remote-degraded-test",
    )
    .await
    .unwrap();
    let status = pairing_state::client::listener_status(&mut client)
        .await
        .unwrap();
    assert!(!status.enabled);
    assert_eq!(status.endpoint_targets, Vec::<String>::new());
    assert!(status.warning.unwrap().contains("already in use"));
    let diagnostic = server.run(&["remote", "status"]);
    assert_success(&diagnostic, "read degraded remote status");
    assert!(String::from_utf8_lossy(&diagnostic.stdout).contains("already in use"));
    // Exercise the local execution path, not just a live IPC endpoint.
    let local = server.run(&["new-session", "--name", "local-during-port-conflict"]);
    assert_success(
        &local,
        "create local terminal while remote port is occupied",
    );
    assert!(
        pairing_commands::client::start_listener(&mut client)
            .await
            .is_err()
    );
    assert!(
        !pairing_state::client::listener_status(&mut client)
            .await
            .unwrap()
            .enabled
    );
    drop(occupied);
    let recovered = pairing_commands::client::start_listener(&mut client)
        .await
        .unwrap();
    assert!(recovered.enabled);
    assert!(recovered.warning.is_none());
    assert!(recovered.endpoint_targets.contains(&format!(
        "device://{}?addr={address}",
        recovered.endpoint_id
    )));
}
