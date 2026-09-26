//! Ordinary typed split must not downgrade directory-aware creation to legacy spawning.
#[allow(dead_code)]
mod support;

use bmux_pane_runtime_plugin_api::{pane_runtime_commands, pane_runtime_state};
use bmux_tabs_plugin_api::tabs_commands;
use std::time::{Duration, Instant};

#[tokio::test]
async fn cli_directory_explanation_does_not_create_executions() {
    let mut server = support::ServerEnv::new("explain-directory");
    server.write_config("[general]\ndefault_shell = '/bin/bash'\n");
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut connection = bmux_client::BmuxClient::connect(
        &endpoint,
        Duration::from_secs(10),
        "explain-directory-test",
    )
    .await
    .unwrap();
    let before = bmux_sessions_plugin_api::sessions_state::client::list_sessions(&mut connection)
        .await
        .unwrap();
    let output = server.run(&[
        "explain-spawn-directory",
        "--operation",
        "pane",
        "--cwd",
        "/execution-only/project",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("invalid explanation JSON: {error}; {:?}", output));
    assert_eq!(report["cwd"], "/execution-only/project");
    assert_eq!(report["policy"]["source"], "path");
    assert_eq!(report["creation_validated"], false);
    let after = bmux_sessions_plugin_api::sessions_state::client::list_sessions(&mut connection)
        .await
        .unwrap();
    assert_eq!(before.len(), after.len());
}

#[tokio::test]
async fn absent_directory_settings_split_reaches_real_shell() {
    use bmux_pane_runtime_plugin_api::attach_runtime_commands::{self, client as attach};

    let mut server = support::ServerEnv::new("spawn-default-shell");
    server.write_config("[general]\ndefault_shell = '/bin/bash'\n");
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut connection =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(10), "spawn-default-test")
            .await
            .unwrap();
    let session = pane_runtime_commands::client::new_session_with_runtime(
        &mut connection,
        Some("default-directory".into()),
    )
    .await
    .unwrap()
    .unwrap();
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
    let before = pane_runtime_state::client::list_panes(&mut connection, Some(session.session_id))
        .await
        .unwrap()
        .unwrap();
    let ack = tabs_commands::client::split_pane(
        &mut connection,
        Some(tabs_commands::Selector {
            id: Some(session.session_id),
            name: None,
            index: None,
        }),
        Some(tabs_commands::Selector {
            id: Some(before.panes[0].id),
            name: None,
            index: None,
        }),
        tabs_commands::PaneDirection::Horizontal,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let pane = ack
        .pane_id
        .expect("ordinary split returns its created pane");
    assert_ne!(pane, before.panes[0].id);
    let report = server.root().join("shell-cwd");
    pane_runtime_commands::client::pane_direct_input(
        &mut connection,
        session.session_id,
        pane,
        format!("printf '%s\\n' \"$PWD\" > '{}'\n", report.display()).into_bytes(),
    )
    .await
    .unwrap()
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let directory = loop {
        if let Ok(text) = std::fs::read_to_string(&report)
            && !text.trim().is_empty()
        {
            break std::path::PathBuf::from(text.trim());
        }
        assert!(
            Instant::now() < deadline,
            "shell did not report CWD: {}",
            server.server_output()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(directory.is_absolute());
    assert!(directory.is_dir());
    let after = pane_runtime_state::client::list_panes(&mut connection, Some(session.session_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.panes.len(), before.panes.len() + 1);
    assert!(after.panes.iter().any(|entry| entry.id == pane));
}

#[tokio::test]
async fn unsupported_directory_aware_split_preserves_existing_runtime() {
    let mut server = support::ServerEnv::new("spawn-directory");
    server.write_config(
        "[general]\ndefault_shell = '/bin/bash'\n\
         [plugins.settings.\"bmux.tabs\".cwd.new_pane]\n\
         source = 'path'\npath = '/tmp'\n",
    );
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut connection = bmux_client::BmuxClient::connect(
        &endpoint,
        Duration::from_secs(10),
        "spawn-directory-test",
    )
    .await
    .unwrap();
    let session = pane_runtime_commands::client::new_session_with_runtime(
        &mut connection,
        Some("existing".into()),
    )
    .await
    .unwrap()
    .unwrap();
    let before = pane_runtime_state::client::list_panes(&mut connection, Some(session.session_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.panes.len(), 1);
    let result = tabs_commands::client::split_pane(
        &mut connection,
        Some(tabs_commands::Selector {
            id: Some(session.session_id),
            name: None,
            index: None,
        }),
        Some(tabs_commands::Selector {
            id: Some(before.panes[0].id),
            name: None,
            index: None,
        }),
        tabs_commands::PaneDirection::Horizontal,
        None,
    )
    .await;
    // Until the runtime advertises safe spawn support, this must be an explicit
    // failure rather than a successful split with silently discarded options.
    let error = match result {
        Ok(Ok(ack)) => panic!("directory options were silently discarded: {ack:?}"),
        Ok(Err(error)) => format!("{error:?}"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("pane-runtime-spawn-commands-v1/split-shell"),
        "unexpected failure: {error}"
    );
    assert!(
        error.contains("no advertised Command service")
            && error.contains("bmux.pane_runtime.spawn.v1")
            && error.contains("was not dispatched"),
        "missing explicit unsupported-interface explanation: {error}"
    );
    let after = pane_runtime_state::client::list_panes(&mut connection, Some(session.session_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.panes.len(), 1);
    assert_eq!(after.panes[0].id, before.panes[0].id);
}
