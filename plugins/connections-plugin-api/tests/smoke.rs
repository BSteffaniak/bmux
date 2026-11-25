use bmux_connections_plugin_api::{
    connection_types::{ConnectionError, InvocationOptions, ServiceInvocation, ServiceInvokeKind},
    connections_commands, hosting_admin_state, hosting_commands, hosting_state, saved_hosts_state,
    hosting_types::{Device, DeviceRole, DeviceState, PairingSession},
};

#[test]
fn invocation_round_trips() {
    let invocation = ServiceInvocation {
        target: "desk".into(),
        capability: "bmux.example.read".into(),
        kind: ServiceInvokeKind::InvokeQuery,
        interface_id: "example/v1".into(),
        operation: "get".into(),
        payload: vec![1, 2, 3],
        options: Some(InvocationOptions {
            timeout_ms: 1_000,
            max_attempts: 1,
            retry_backoff_ms: 0,
        }),
    };
    let json = serde_json::to_string(&invocation).unwrap();
    assert_eq!(
        serde_json::from_str::<ServiceInvocation>(&json).unwrap(),
        invocation
    );
    let error = ConnectionError::TargetNotFound {
        target: "desk".into(),
    };
    assert_eq!(
        serde_json::from_str::<ConnectionError>(&serde_json::to_string(&error).unwrap()).unwrap(),
        error
    );
}

#[test]
fn hosting_interfaces_are_distinct_and_devices_round_trip() {
    let ids = [
        connections_commands::INTERFACE_ID.to_string(),
        hosting_commands::INTERFACE_ID.to_string(),
        hosting_admin_state::INTERFACE_ID.to_string(),
        hosting_state::INTERFACE_ID.to_string(),
        saved_hosts_state::INTERFACE_ID.to_string(),
    ];
    for (index, id) in ids.iter().enumerate() {
        assert!(!ids[index + 1..].contains(id));
    }
    let device = Device {
        device_key: "key".into(),
        principal: uuid::Uuid::from_u128(7),
        name: "laptop".into(),
        state: DeviceState::Active,
        role: DeviceRole::ViewOnly,
        revision: 2,
        approved_at: Some(1),
        last_seen: None,
    };
    assert_eq!(
        serde_json::from_str::<Device>(&serde_json::to_string(&device).unwrap()).unwrap(),
        device
    );
}

#[test]
fn pairing_link_is_secret_in_debug_output() {
    let session = PairingSession {
        session_id: "s".into(),
        link: "bmux-pair:secretmaterial".into(),
        expires_at: 1,
    };
    assert!(!format!("{session:?}").contains("secretmaterial"));
}

#[test]
fn administration_is_owner_local_and_status_is_observe() {
    use bmux_plugin_sdk::RouteAccess;
    assert_eq!(hosting_commands::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(hosting_admin_state::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(connections_commands::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(saved_hosts_state::ACCESS, Some(RouteAccess::OwnerLocal));
    assert_eq!(hosting_state::ACCESS, Some(RouteAccess::Observe));
}
