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
            "missing {expected}; handshake path={}; contents={:?}; server output: {}",
            path.display(),
            std::fs::read_to_string(path),
            server.server_output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_restores(path: &std::path::Path, count: usize, server: &support::ServerEnv) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.lines().filter(|line| *line == "committed").count() == count + 1 {
            let welcomes = text
                .lines()
                .filter(|line| *line != "committed")
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(welcomes.len(), count + 1);
            assert!(welcomes[0]["restore"].is_null());
            for welcome in &welcomes[1..] {
                assert_eq!(welcome["restore"]["application"], "test");
                assert_eq!(
                    welcome["restore"]["session"],
                    format!("saved-selection:{}", path.display()),
                    "restoration selected another pane's application session"
                );
                assert_eq!(welcome["cwd"], welcomes[0]["cwd"], "restart changed cwd");
            }
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing restoration {count}: {text}; {}",
            server.server_output()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn layout_root(
    connection: &mut bmux_client::BmuxClient,
    session: uuid::Uuid,
) -> serde_json::Value {
    use bmux_pane_runtime_plugin_api::attach_runtime_commands::{self, client as attach};
    let grant = attach::attach_session(
        connection,
        attach_runtime_commands::SessionSelector {
            id: Some(session),
            name: None,
        },
        true,
    )
    .await
    .unwrap()
    .unwrap();
    attach::attach_open(connection, session, grant.token)
        .await
        .unwrap()
        .unwrap();
    let layout = bmux_pane_runtime_plugin_api::attach_runtime_state::client::attach_layout_state(
        connection, session,
    )
    .await
    .unwrap()
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&layout.encoded).unwrap();
    value.get("layout_root").expect("layout root").clone()
}

fn bash_path() -> String {
    let resolved = std::process::Command::new("which")
        .arg("bash")
        .output()
        .expect("resolve bash from PATH");
    assert!(
        resolved.status.success(),
        "bash required for restart coverage"
    );
    String::from_utf8(resolved.stdout)
        .expect("bash path is UTF-8")
        .trim()
        .to_owned()
}

#[tokio::test]
async fn application_session_survives_real_daemon_restart() {
    round_trip(false, &bash_path()).await;
}

#[tokio::test]
async fn shell_application_session_survives_real_daemon_restart() {
    round_trip(true, &bash_path()).await;
}

#[tokio::test]
async fn fish_application_session_survives_real_daemon_restart() {
    let resolved = std::process::Command::new("which")
        .arg("fish")
        .output()
        .expect("resolve fish from PATH");
    assert!(
        resolved.status.success(),
        "fish required for restart coverage"
    );
    let fish = String::from_utf8(resolved.stdout).unwrap();
    round_trip(true, fish.trim()).await;
}

#[tokio::test]
async fn nushell_application_session_survives_real_daemon_restart() {
    let resolved = std::process::Command::new("which")
        .arg("nu")
        .output()
        .unwrap();
    if !resolved.status.success() {
        eprintln!("Nushell restart coverage skipped: nu is not installed");
        return;
    }
    let nu = String::from_utf8(resolved.stdout).unwrap();
    round_trip(true, nu.trim()).await;
}

#[tokio::test]
async fn application_sessions_survive_repeated_sigkill_restarts() {
    restart_round_trip(false, &bash_path(), true, 2, 2).await;
}

#[tokio::test]
async fn shell_application_session_survives_repeated_sigkill_restarts() {
    restart_round_trip(true, &bash_path(), true, 1, 2).await;
}

async fn round_trip(shell: bool, shell_program: &str) {
    restart_round_trip(shell, shell_program, false, 1, 1).await;
}

async fn restart_round_trip(
    shell: bool,
    shell_program: &str,
    abrupt: bool,
    pane_count: usize,
    restarts: usize,
) {
    nested_round_trip(shell, shell_program, abrupt, pane_count, restarts, &[]).await;
}

#[tokio::test]
async fn nested_nushell_session_survives_repeated_restarts_and_manual_launch() {
    nested_round_trip(true, &bash_path(), false, 1, 2, &["nu"]).await;
}

#[tokio::test]
async fn deeply_nested_shells_survive_restart() {
    nested_round_trip(true, &bash_path(), false, 1, 2, &["nu", "nu"]).await;
}

#[tokio::test]
async fn nested_fish_survives_restart() {
    nested_round_trip(true, &bash_path(), false, 1, 1, &["fish"]).await;
}

#[tokio::test]
async fn nested_bash_survives_restart() {
    nested_round_trip(true, &bash_path(), false, 1, 1, &["bash"]).await;
}

#[tokio::test]
async fn nested_zsh_survives_restart() {
    if !std::process::Command::new("which")
        .arg("zsh")
        .status()
        .unwrap()
        .success()
    {
        return;
    }
    nested_round_trip(true, &bash_path(), false, 1, 1, &["zsh"]).await;
}

#[tokio::test]
async fn nested_shell_survives_repeated_sigkill() {
    nested_round_trip(true, &bash_path(), true, 1, 2, &["nu"]).await;
}

async fn nested_round_trip(
    shell: bool,
    shell_program: &str,
    abrupt: bool,
    pane_count: usize,
    restarts: usize,
    nested: &[&str],
) {
    let mut server = support::ServerEnv::new("application-resurrection");
    server.isolate_shell_config();
    // Supported startup integration for shells without an inherited vendor
    // autoload directory. No user startup files are modified by the product.
    std::fs::write(server.root().join("home/.bashrc"), r#"if [[ -n ${BMUX_ROOT_SHELL_PID:-} && $BMUX_ROOT_SHELL_PID != $$ && -n ${BMUX_SHELL_CALLBACK:-} ]]; then
  eval "$("$BMUX_SHELL_CALLBACK" __shell-integration bash)"
fi
"#).unwrap();
    std::fs::write(
        server.root().join("home/.zshrc"),
        r#"if [[ -n ${BMUX_SHELL_CALLBACK:-} ]]; then
  eval "$("$BMUX_SHELL_CALLBACK" __shell-integration zsh)"
fi
"#,
    )
    .unwrap();
    server.write_config(&format!(
        "[general]\ndefault_shell = '{shell_program}'\n[behavior]\nrestore_last_layout = true\n",
    ));
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
welcome['cwd'] = os.getcwd()
with open(sys.argv[1], 'a') as out: out.write(json.dumps(welcome)+'\n')
send(dict(operation='selection',revision=1,descriptor=dict(application='test',session='saved-selection:' + sys.argv[1])))
ack = json.loads(f.readline())
if ack != {'revision': 1}:
    raise RuntimeError('selection was not acknowledged: ' + repr(ack))
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
        for nested_shell in nested {
            client::pane_direct_input(
                &mut connection,
                session.session_id,
                pane,
                format!("{nested_shell}\n").into_bytes(),
            )
            .await
            .unwrap()
            .unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        client::pane_direct_input(
            &mut connection,
            session.session_id,
            pane,
            format!(
                "{}'{}' '{}' '{}'\n",
                if nested.last() == Some(&"nu")
                    || (nested.is_empty() && shell_program.ends_with("/nu"))
                {
                    "^"
                } else {
                    ""
                },
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
        for index in 0..pane_count {
            let result = if index == 0 {
                result.clone()
            } else {
                server.root().join(format!("offers-{index}.jsonl"))
            };
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
                Some(server.root().to_string_lossy().into_owned()),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
    let results = (0..pane_count)
        .map(|index| {
            if index == 0 {
                result.clone()
            } else {
                server.root().join(format!("offers-{index}.jsonl"))
            }
        })
        .collect::<Vec<_>>();
    for result in &results {
        wait_for(result, "committed", &server).await;
    }
    if shell {
        // Application selection commits over a different socket from shell
        // metadata. Do not snapshot until both authorities observed the launch.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let panes = bmux_pane_runtime_plugin_api::pane_runtime_state::client::list_panes(
                &mut connection,
                Some(session.session_id),
            )
            .await
            .unwrap()
            .unwrap();
            if panes.panes.iter().any(|pane| {
                pane.active_command.as_deref().is_some_and(|command| {
                    if !nested.is_empty() {
                        command == nested[0]
                    } else {
                        command.contains(&script.to_string_lossy().to_string())
                    }
                })
            }) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shell launch metadata missing before snapshot: {panes:?}; {}",
                server.server_output()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let before = bmux_pane_runtime_plugin_api::pane_runtime_state::client::list_panes(
        &mut connection,
        Some(session.session_id),
    )
    .await
    .unwrap()
    .unwrap();
    let before_layout = layout_root(&mut connection, session.session_id).await;
    drop(connection);
    for restart in 1..=restarts {
        if abrupt {
            // Selection acknowledgements fence the application checkpoint, not
            // the layout snapshot. Commit both before deliberately losing the daemon.
            let saved = server.run(&["server", "save"]);
            assert!(saved.status.success(), "snapshot failed: {saved:?}");
            server.restart();
        } else {
            server.stop();
            server.start();
        }
        for result in &results {
            wait_for_restores(result, restart, &server).await;
        }
        let mut connection = bmux_client::BmuxClient::connect(
            &endpoint,
            Duration::from_secs(10),
            "resurrection-layout-test",
        )
        .await
        .unwrap();
        assert_eq!(
            layout_root(&mut connection, session.session_id).await,
            before_layout,
            "restart changed the pane split layout"
        );
        let after = bmux_pane_runtime_plugin_api::pane_runtime_state::client::list_panes(
            &mut connection,
            Some(session.session_id),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            before.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            after.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
            "restart changed pane identities or ordering"
        );
    }
    if shell {
        manual_launch_after_restore(
            &endpoint,
            session.session_id,
            &python,
            &script,
            &result,
            &server,
            (
                nested.last() == Some(&"nu")
                    || (nested.is_empty() && shell_program.ends_with("/nu")),
                nested.first().copied(),
            ),
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
    shell: (bool, Option<&str>),
) {
    let (nushell, outer) = shell;
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
        let outer_command = panes.panes[0].active_command.as_deref();
        let nested_prompt = if outer.is_some() {
            let path = server
                .state_dir
                .join("application-resurrection")
                .join(format!("{pane}.json"));
            std::fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|mut checkpoint| {
                    while let Some(child) = checkpoint["commands"]
                        .as_array()
                        .and_then(|items| items.first())
                    {
                        checkpoint = child["checkpoint"].clone();
                    }
                    checkpoint["descriptor"].is_null()
                })
        } else {
            outer_command.is_none()
        };
        if nested_prompt && (outer_command.is_none() || (outer.is_some() && outer_command == outer))
        {
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
            "{}'{python}' '{}' '{}'\n",
            if nushell { "^" } else { "" },
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
