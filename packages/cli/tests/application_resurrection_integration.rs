//! Real daemon/PTY/snapshot round trip with an application-protocol participant.
#[allow(dead_code)]
mod support;

use bmux_pane_runtime_plugin_api::pane_runtime_commands::{self, client};
use std::time::{Duration, Instant};

async fn wait_for(path: &std::path::Path, expected: &str, server: &support::ServerEnv) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|text| text.contains(expected)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing {expected}: {}",
            server.server_output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn application_session_survives_real_daemon_restart() {
    round_trip(false).await;
}

#[tokio::test]
async fn shell_application_session_survives_real_daemon_restart() {
    round_trip(true).await;
}

async fn round_trip(shell: bool) {
    let mut server = support::ServerEnv::new("application-resurrection");
    server.write_config(
        "[general]\ndefault_shell = '/bin/bash'\n[behavior]\nrestore_last_layout = true\n",
    );
    let python = std::process::Command::new("which")
        .arg("python3")
        .output()
        .unwrap();
    assert!(
        python.status.success(),
        "python3 required for protocol process fixture"
    );
    let python = String::from_utf8(python.stdout).unwrap().trim().to_owned();
    let script = server.root().join("application.py");
    let result = server.root().join("offers.jsonl");
    std::fs::write(&script, r#"import json, os, socket, sys, time
s = socket.socket(socket.AF_UNIX)
s.connect(os.environ['APPLICATION_RESURRECTION_HOST'][3:])
f = s.makefile('rwb', buffering=0)
def send(value): f.write((json.dumps(value)+'\n').encode())
send(dict(protocol='application-resurrection/1',application='test',pid=os.getpid(),accept_restore=True))
welcome = json.loads(f.readline())
with open(sys.argv[1], 'a') as out: out.write(json.dumps(welcome)+'\n')
send(dict(operation='selection',revision=1,descriptor=dict(application='test',session='saved-selection')))
ack = json.loads(f.readline())
with open(sys.argv[1], 'a') as out: out.write('committed\n')
while True: time.sleep(1)
"#).unwrap();
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut connection =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(10), "resurrection-test")
            .await
            .unwrap();
    let session = client::new_session_with_runtime(&mut connection, Some("recovery".into()))
        .await
        .unwrap()
        .unwrap();
    if shell {
        use bmux_pane_runtime_plugin_api::attach_runtime_commands::{self, client as attach};
        let grant = attach::attach_session(
            &mut connection,
            attach_runtime_commands::SessionSelector {
                id: Some(session.session_id),
                name: None,
            },
            true,
        )
        .await
        .unwrap()
        .unwrap();
        attach::attach_open(&mut connection, session.session_id, grant.token)
            .await
            .unwrap()
            .unwrap();
        let panes = bmux_pane_runtime_plugin_api::pane_runtime_state::client::list_panes(
            &mut connection,
            Some(session.session_id),
        )
        .await
        .unwrap()
        .unwrap();
        let pane = panes.panes[0].id;
        // Wait for shell startup before injecting a command through the public input service.
        tokio::time::sleep(Duration::from_secs(1)).await;
        client::pane_direct_input(
            &mut connection,
            session.session_id,
            pane,
            format!(
                "'{}' '{}' '{}'\n",
                python,
                script.display(),
                result.display()
            )
            .into_bytes(),
        )
        .await
        .unwrap()
        .unwrap();
    } else {
        let _: pane_runtime_commands::PaneAck = client::launch_pane(
            &mut connection,
            session.session_id,
            None,
            "horizontal".into(),
            50,
            None,
            python.clone(),
            vec![
                script.to_string_lossy().into_owned(),
                result.to_string_lossy().into_owned(),
            ],
            None,
        )
        .await
        .unwrap()
        .unwrap();
    }
    wait_for(&result, "committed", &server).await;
    drop(connection);
    server.stop();
    server.start();
    wait_for(&result, "saved-selection", &server).await;
    if shell {
        manual_launch_after_restore(
            &endpoint,
            session.session_id,
            &python,
            &script,
            &result,
            &server,
        )
        .await;
    }
    server.stop();
}

async fn manual_launch_after_restore(
    endpoint: &bmux_ipc::IpcEndpoint,
    session: uuid::Uuid,
    python: &str,
    script: &std::path::Path,
    result: &std::path::Path,
    server: &support::ServerEnv,
) {
    use bmux_pane_runtime_plugin_api::attach_runtime_commands::{self, client as attach};
    use bmux_pane_runtime_plugin_api::pane_runtime_state::client as state;
    let mut connection =
        bmux_client::BmuxClient::connect(endpoint, Duration::from_secs(10), "manual-launch-test")
            .await
            .unwrap();
    let grant = attach::attach_session(
        &mut connection,
        attach_runtime_commands::SessionSelector {
            id: Some(session),
            name: None,
        },
        true,
    )
    .await
    .unwrap()
    .unwrap();
    attach::attach_open(&mut connection, session, grant.token)
        .await
        .unwrap()
        .unwrap();
    let pane = state::list_panes(&mut connection, Some(session))
        .await
        .unwrap()
        .unwrap()
        .panes[0]
        .id;
    client::pane_direct_input(&mut connection, session, pane, vec![3])
        .await
        .unwrap()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let panes = state::list_panes(&mut connection, Some(session))
            .await
            .unwrap()
            .unwrap();
        if panes.panes[0].active_command.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restored application did not return to prompt"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let manual_result = result.with_extension("manual.jsonl");
    client::pane_direct_input(
        &mut connection,
        session,
        pane,
        format!(
            "'{python}' '{}' '{}'\n",
            script.display(),
            manual_result.display()
        )
        .into_bytes(),
    )
    .await
    .unwrap()
    .unwrap();
    wait_for(&manual_result, "committed", server).await;
    let text = std::fs::read_to_string(manual_result).unwrap();
    let welcome: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert!(
        welcome["restore"].is_null(),
        "manual invocation consumed stale restoration offer: {welcome}"
    );
}
