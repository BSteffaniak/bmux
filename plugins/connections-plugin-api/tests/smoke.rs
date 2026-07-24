use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, ConnectionTransport, InvocationOptions, ResolvedEndpoint},
    connections_commands, connections_state,
};

#[test]
fn endpoint_round_trips() {
    let endpoint = ResolvedEndpoint {
        endpoint_id: "prod".to_string(),
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
fn invocation_policy_and_errors_round_trip() {
    let options = InvocationOptions {
        timeout_ms: 5_000,
        max_attempts: 3,
        retry_backoff_ms: 25,
    };
    let json = serde_json::to_string(&options).expect("serialize invocation options");
    assert_eq!(
        serde_json::from_str::<InvocationOptions>(&json).expect("deserialize invocation options"),
        options
    );

    for error in [
        ConnectionError::Cancelled {
            target: "prod".to_string(),
        },
        ConnectionError::TimedOut {
            target: "prod".to_string(),
            phase: "acquire".to_string(),
            timeout_ms: 5_000,
        },
        ConnectionError::RetryExhausted {
            target: "prod".to_string(),
            attempts: 3,
            reason: "offline".to_string(),
        },
    ] {
        let json = serde_json::to_string(&error).expect("serialize connection error");
        assert_eq!(
            serde_json::from_str::<ConnectionError>(&json).expect("deserialize connection error"),
            error
        );
    }
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
