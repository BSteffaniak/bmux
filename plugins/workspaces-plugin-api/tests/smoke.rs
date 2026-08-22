use bmux_workspaces_plugin_api::{
    workspaces_commands::WorkspaceAck,
    workspaces_events::{self, WorkspaceEvent},
    workspaces_list,
    workspaces_state::{self, WorkspaceSelector, WorkspaceSummary},
};

#[test]
fn generated_workspace_contract_round_trips() {
    let workspace = WorkspaceSummary {
        id: uuid::Uuid::nil(),
        name: "default".to_string(),
        tab_ids: vec![uuid::Uuid::from_u128(1)],
        active: true,
    };
    let encoded = serde_json::to_string(&workspace).expect("workspace should encode");
    let decoded: WorkspaceSummary =
        serde_json::from_str(&encoded).expect("workspace should decode");
    assert_eq!(decoded, workspace);

    let selector = WorkspaceSelector {
        id: Some(workspace.id),
        name: None,
    };
    assert!(serde_json::to_string(&selector).unwrap().contains("id"));

    let ack = WorkspaceAck {
        id: workspace.id,
        selected_context_id: workspace.tab_ids.first().copied(),
    };
    assert_eq!(ack.selected_context_id, Some(uuid::Uuid::from_u128(1)));
}

#[test]
fn generated_workspace_interfaces_are_namespaced() {
    assert_eq!(workspaces_state::INTERFACE_ID, "workspaces-state");
    assert_eq!(workspaces_list::INTERFACE_ID, "workspaces-list");
    assert_eq!(
        workspaces_events::EVENT_KIND,
        "bmux.workspaces/workspaces-events"
    );
    let event = WorkspaceEvent::Removed {
        workspace_id: uuid::Uuid::nil(),
    };
    assert!(serde_json::to_string(&event).unwrap().contains("removed"));
}
