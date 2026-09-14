#[allow(dead_code)] // Shared subprocess fixture also supports gateway-specific tests.
mod support;

use bmux_ipc::InvokeServiceKind;
use bmux_tabs_plugin_api::tabs_commands::client::NewTabRequest;
use bmux_workspaces_plugin_api::workspaces_commands::client::{
    NewWorkspaceRequest, SwitchWorkspaceRequest,
};
use bmux_workspaces_plugin_api::workspaces_state::WorkspaceSelector;
use std::time::Duration;

async fn call<Q: serde::Serialize, R: serde::de::DeserializeOwned>(
    client: &mut bmux_client::BmuxClient,
    capability: &str,
    interface: &str,
    operation: &str,
    payload: &Q,
) -> R {
    let bytes = bmux_plugin_sdk::encode_service_message(payload).unwrap();
    let bytes = client
        .invoke_service_raw(
            capability,
            InvokeServiceKind::Command,
            interface,
            operation,
            bytes,
        )
        .await
        .unwrap_or_else(|error| panic!("{interface}/{operation}: {error}"));
    bmux_plugin_sdk::decode_service_message(&bytes).unwrap()
}
type WorkspaceResult = Result<
    bmux_workspaces_plugin_api::workspaces_commands::WorkspaceAck,
    bmux_workspaces_plugin_api::workspaces_commands::WorkspaceCommandError,
>;
type TabResult = Result<
    bmux_tabs_plugin_api::tabs_commands::TabAck,
    bmux_tabs_plugin_api::tabs_commands::TabError,
>;
#[derive(serde::Serialize)]
struct GroupRequest {
    action: String,
    tabs: Vec<uuid::Uuid>,
    workspace: uuid::Uuid,
}

#[tokio::test]
async fn real_group_move_keeps_server_responsive() {
    let mut server = support::ServerEnv::new("group-move");
    server.start();
    let endpoint = bmux_ipc::IpcEndpoint::UnixSocket(server.runtime_dir.join("server.sock"));
    let mut client =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(5), "group-move-test")
            .await
            .unwrap();
    eprintln!("isolated root: {}", server.root().display());
    let source: WorkspaceResult = call(
        &mut client,
        "bmux.workspaces.write",
        "workspaces-commands",
        "new-workspace",
        &NewWorkspaceRequest {
            name: Some("source".into()),
        },
    )
    .await;
    let source = source.unwrap().id;
    let mut ids = Vec::new();
    for name in ["one", "two", "three"] {
        let tab: TabResult = call(
            &mut client,
            "bmux.tabs.write",
            "tabs-commands",
            "new-tab",
            &NewTabRequest {
                name: Some(name.into()),
            },
        )
        .await;
        eprintln!("tab={tab:?}");
        ids.push(uuid::Uuid::parse_str(&tab.unwrap().id.unwrap()).unwrap());
    }
    let destination: WorkspaceResult = call(
        &mut client,
        "bmux.workspaces.write",
        "workspaces-commands",
        "new-workspace",
        &NewWorkspaceRequest {
            name: Some("destination".into()),
        },
    )
    .await;
    let destination = destination.unwrap().id;
    let _: WorkspaceResult = call(
        &mut client,
        "bmux.workspaces.write",
        "workspaces-commands",
        "switch-workspace",
        &SwitchWorkspaceRequest {
            selector: WorkspaceSelector {
                id: Some(source),
                name: None,
            },
        },
    )
    .await;
    let mut probe =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(5), "group-probe")
            .await
            .unwrap();
    let probing = tokio::spawn(async move {
        for _ in 0..40 {
            let response = probe
                .invoke_service_raw(
                    "bmux.tabs.read",
                    InvokeServiceKind::Query,
                    "tabs-state",
                    "list-tabs",
                    bmux_plugin_sdk::encode_service_message(
                        &bmux_tabs_plugin_api::tabs_state::client::ListTabsRequest {
                            session: None,
                        },
                    )
                    .unwrap(),
                )
                .await;
            if response.is_err() {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    });
    let mut result = Ok(Vec::new());
    for iteration in 0..20 {
        let bytes = bmux_plugin_sdk::encode_service_message(&GroupRequest {
            action: "move".into(),
            tabs: ids.clone(),
            workspace: if iteration % 2 == 0 {
                destination
            } else {
                source
            },
        })
        .unwrap();
        result = client
            .invoke_service_raw(
                "bmux.tab_bar.input",
                InvokeServiceKind::Command,
                "presentation-input",
                "move-selection",
                bytes,
            )
            .await;
        if result.is_err() {
            break;
        }
    }
    eprintln!("move result: {result:?}");
    if result.is_err() {
        eprintln!("{}", server.server_output());
        #[cfg(target_os = "macos")]
        if let Some(pid) = server.server_pid() {
            let output = std::process::Command::new("/usr/bin/sample")
                .args([
                    pid.to_string(),
                    "1".into(),
                    "1".into(),
                    "-file".into(),
                    "/tmp/bmux-group-hang.sample".into(),
                ])
                .output()
                .unwrap();
            eprintln!("sample: {:?}", output.status);
        }
    }
    let probe_ok = probing.await.unwrap();
    assert!(
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(5), "reconnect-after-move")
            .await
            .is_ok()
    );
    server.kill();
    assert!(result.is_ok());
    assert!(probe_ok);
    server.start();
    let mut recovered =
        bmux_client::BmuxClient::connect(&endpoint, Duration::from_secs(5), "after-restart")
            .await
            .unwrap();
    let bytes = recovered
        .invoke_service_raw(
            "bmux.workspaces.read",
            InvokeServiceKind::Query,
            "workspaces-state",
            "get-workspace",
            bmux_plugin_sdk::encode_service_message(
                &bmux_workspaces_plugin_api::workspaces_state::client::GetWorkspaceRequest {
                    selector: WorkspaceSelector {
                        id: Some(source),
                        name: None,
                    },
                },
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let workspace: Result<
        bmux_workspaces_plugin_api::workspaces_state::WorkspaceSummary,
        bmux_workspaces_plugin_api::workspaces_state::WorkspaceQueryError,
    > = bmux_plugin_sdk::decode_service_message(&bytes).unwrap();
    // Workspace catalog is durable; unsnapshotted local PTYs/contexts are not
    // adopted by a freshly restarted server.
    assert_eq!(workspace.unwrap().id, source);
    server.kill();
}
