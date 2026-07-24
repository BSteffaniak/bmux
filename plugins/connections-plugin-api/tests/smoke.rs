use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, ConnectionTransport, ResolvedEndpoint},
    connections_commands, connections_state,
};

#[test]
fn endpoint_round_trips() {
    let endpoint = ResolvedEndpoint {
        reference: "prod".to_string(),
        label: "prod".to_string(),
        transport: ConnectionTransport::Ssh,
        address: "ops@example.com:22".to_string(),
        server_name: None,
        connect_timeout_ms: 8_000,
    };
    let json = serde_json::to_string(&endpoint).expect("serialize endpoint");
    assert_eq!(
        serde_json::from_str::<ResolvedEndpoint>(&json).expect("deserialize endpoint"),
        endpoint
    );
}

#[test]
fn errors_are_structured() {
    let error = ConnectionError::TargetNotFound {
        target: "missing".to_string(),
    };
    let json = serde_json::to_string(&error).expect("serialize error");
    assert!(json.contains("target_not_found"));
}

#[test]
fn interface_ids_are_stable() {
    assert_eq!(connections_state::INTERFACE_ID, "connections-state");
    assert_eq!(connections_commands::INTERFACE_ID, "connections-commands");
    assert_eq!(connections_state::OP_RESOLVE, "resolve");
    assert_eq!(connections_commands::OP_INVOKE_SERVICE, "invoke-service");
}

#[test]
fn contract_declares_both_services() {
    assert_eq!(
        bmux_connections_plugin_api::service_declarations()
            .expect("valid declarations")
            .len(),
        2
    );
}
